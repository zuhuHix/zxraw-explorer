# Testing and benchmarks

## Running tests

- While iterating, run `scripts/test-affected.ps1` (PowerShell 5.1). It diffs against the
  merge-base with `origin/main`, plus uncommitted and untracked files, then runs the unit tests of
  the changed crates and the crates that depend on them, and only the e2e areas the change
  reaches. The plan is printed first. `-DryRun` prints the plan without running anything,
  `-Base <ref>` compares against another ref, and `-All` runs the whole workspace.
- Run the full `cargo test --workspace` only before a merge or a release.
- E2E runs up to `ZXRAW_E2E_JOBS` app windows at once (default `min(3, cores / 2)`, at least 1).
  `ZXRAW_E2E_JOBS=1` restores one at a time. Timing tests (`perf::` and
  `browser::two_thousand_small_images_record_grouping_frame_times`) always run alone.
- Tests that use the real Windows clipboard (forge-win's clipboard unit tests and the e2e
  clipboard scenarios in `core`) take the named mutex `Local\ZXrawExplorerClipboardTests`,
  so they run one at a time across test binaries and worktrees. The e2e ones save the
  clipboard first and put it back afterwards, unless something other than the test changed it.
- To run one area, name it after `--`: `cargo test -p forge-app --test e2e -- library::`.
  Several areas can be listed. The areas are browser, core, ingest, library, media, perf,
  related, sidebar, system, tools and viewer.

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

`forge-app/src/testhook/mod.rs` reads these environment variables (script mode is described under [End-to-end tests](#end-to-end-tests)):

* `ZXRAW_CMDS` is a comma-separated list of `Cmd` debug names, for example
  `ViewGrid,ToggleDualPane` or `ViewDetails` (see `crates/forge-app/src/commands.rs`).
  Commands run once the initial folder load completes and at least five frames have run.
* `ZXRAW_SELECT` selects a matching filename in the focused pane before those commands.
* `ZXRAW_SCREENSHOT` saves an egui-rendered PNG and then closes the app. The screenshot is
  requested on the first frame after frame 90 counted from startup, not 90 frames after the
  commands ran. The PNG is written a few frames later.
* `ZXRAW_INSTANCE_NAME=<tag>` (debug and `automation` builds) gives the run its own single-instance
  pipe. Automated runs take no part in single-window hand-off without it. Debug builds otherwise use a
  `-dev` pipe, so a dev run never forwards to or receives from the installed copy.
* `ZXRAW_FORWARD_ONLY=1` (debug and `automation` builds) makes a launch exit with code 3 instead of
  opening a window when it cannot hand over. Tests use it.

The hook has **no timeout**: if a listing never finishes, the app waits indefinitely. Wrap
runs in a watchdog (as in the example below).

The automation hook and profile override are compiled into debug builds. Release builds
include them only when the non-default `automation` feature is explicitly enabled. The
release packaging script uses `--no-default-features` and does not enable this feature.
The end-to-end suite runs the debug `zxraw` binary, so it does not need the feature.

Example PowerShell session (the 50,000-file scenario used for [BENCHMARKS.md](BENCHMARKS.md)):

```powershell
$env:CARGO_TARGET_DIR = 'target/l5'
cargo build -p forge-app --release --no-default-features --features automation
$png = "$env:TEMP\zxraw-bench\scenario-flat50k-details.png"
$env:ZXRAW_SCREENSHOT = $png
$env:ZXRAW_CMDS = 'ViewDetails'
$p = Start-Process -FilePath '.\target\l5\release\zxraw.exe' -ArgumentList @("$env:TEMP\zxraw-bench\flat50k") -PassThru
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not (Test-Path $png) -and -not $p.HasExited -and $sw.Elapsed.TotalSeconds -lt 300) { Start-Sleep -Milliseconds 200 }
Remove-Item Env:ZXRAW_SCREENSHOT, Env:ZXRAW_CMDS -ErrorAction Ignore
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
```

Without `ZXRAW_PROFILE_DIR`, automation uses a unique throwaway profile for this process,
so the screenshot does not include the user's session-restore tabs. The screenshot hook is
a smoke-test aid and does not assert visual correctness. Script mode (below) injects real key and pointer input.

## Archive fuzzing

Archives come from the internet, so every parser in `forge-core/src/archive/` is fuzzed. The
targets are defined once, in `forge-core/src/archive/fuzz.rs` (`Target`, `run`). Two drivers
use them: the libFuzzer targets in `fuzz/`, and the stable test
`crates/forge-core/tests/archive_fuzz.rs`. The module is compiled only with forge-core's
`fuzz-harness` feature, which the fuzz crate and forge-core's own tests enable (a
dev-dependency on itself); the app never includes it.

| Target | What it feeds |
|---|---|
| `detect` | content detection (`backend_for_file`), then the full pipeline of whatever it picked |
| `zip`, `sevenz`, `tar`, `rar` | the full pipeline under that extension: list, preflight, check password, `read_entries` into a sink, then *Test* (`read_entries_checked`); each with no password and with `secret` (the fixtures' password) |
| `stream` | the same, for `.tar.<codec>` or a single `.<codec>` file (the first byte picks gz/bz2/xz/zst/lz4/br, tar or not) |
| `zip_peek` | the ZIP encryption peek (`crypto_peek`: WinZip AES and ZipCrypto heads) |
| `sevenz_guard` | the 7z header guard alone (`sevenz_guard::check`) |
| `tar_walk` | the tar header walk over an in-memory slice (`TarParser`) |
| `codec_walk` | a codec's header walk (`codec::stream_info`, full and peek caps) |

Every target runs under tight limits (`fuzz::limits()`): 1,000 entries, 64 MiB in total,
16 MiB per file and 64 MiB of decoder memory. Both drivers install a tracking allocator.
A single allocation over 256 MiB (fuzz) or 512 MiB (test) aborts at once. The test also
fails an input that keeps more than 512 MiB live. It runs each input on a thread of its
own and waits at most 20 s for it: an input that hangs fails the test at once (the test
stops there and leaves that thread spinning until the process ends).
libFuzzer's `-rss_limit_mb=2048` covers allocations in C code (zstd, UnRAR), and
`-timeout=10` catches hangs.

### Stable: part of `cargo test`

```powershell
cargo test -p forge-core --test archive_fuzz
```

* `replay_regressions` replays every input committed under `fuzz/regressions/<target>/`.
  These are the minimised crashes and hangs the fuzzers found.
* `mutation_smoke` is a fixed-seed mutation loop (xorshift: bit flips, interesting values,
  deletes, duplicates, truncation, splices). It runs over seed archives: the committed
  fixtures, plus zip/7z/tar/codec archives the engine writes. That is a few seconds per
  target and about 20 s in total in a debug build. A failing input is saved under
  `%TEMP%\zxraw-fuzz-failures\<target>\`.
* `ZXRAW_FUZZ_TRACE=1` prints each pipeline step with its time and the listing, which helps
  locate a hang.

### libFuzzer (nightly, coverage-guided)

One-time setup: `rustup toolchain install nightly` and `cargo install cargo-fuzz`, plus the
Visual Studio C++ tools, which ship the ASan runtime DLL. Then, from the repository root:

```powershell
fuzz\run.ps1 -Seconds 1200 -Jobs 4                    # every target, 20 min each, 4 at a time
fuzz\run.ps1 -Targets sevenz,zip -Seconds 300 -NoBuild
```

The script builds with `cargo +nightly fuzz build`, which uses AddressSanitizer. It puts
`clang_rt.asan_dynamic-x86_64.dll` on `PATH` and runs each target for `-Seconds`. After a
crash it restarts the target until that time is used up. Its outputs:

* crashes, timeouts and OOMs go to `fuzz/artifacts/<target>/`;
* logs go to `fuzz/logs/<target>.log`;
* the corpus grows in `fuzz/corpus/<target>/`, seeded from `fuzz/seeds/<target>/`.

Artifacts, logs and the corpus are not committed. Keep `-Jobs` under 5, because Defender
adds heavy load to parallel builds. To regenerate the seeds:
`cargo test -p forge-core --test archive_fuzz -- --ignored write_fuzz_seeds`.

Two Windows quirks are handled by the setup:

* sevenz-rust2 also builds a `cdylib`, which can't link with coverage instrumentation.
  `run.ps1` copies the crate to `fuzz/vendor/` as an rlib only, and `fuzz/Cargo.toml`
  patches it in.
* `-s none` (no sanitizer) fails to link on MSVC (`__start___sancov_cntrs`), so keep the
  default ASan.

To replay one input: `fuzz\target\x86_64-pc-windows-msvc\release\<target>.exe <file>`.
Then minimise it with `-minimize_crash=1 -runs=10000`, and commit it to
`fuzz/regressions/<target>/` together with the fix.

Running time so far (2026-11-03, i7-14700K, 4 targets at a time):

| Target | Time | Executions | Findings |
|---|---:|---:|---|
| sevenz | 40 min | 41,778 | the LZMA2 hang below (8 inputs) |
| detect | 40 min | 88,390 | none |
| rar | 40 min | 28,585 | none |
| stream | 40 min | 120,603 | none |
| zip | 20 min | 104,821 | none |
| tar | 20 min | 147,998 | none |
| sevenz_guard | 20 min | 53,464 | none |
| zip_peek | 20 min | 52,894 | none |
| tar_walk | 20 min | 4,733,929 | none |
| codec_walk | 20 min | 259,446 | none |

Found and fixed: lzma-rust2 0.23's `Lzma2Reader` loops forever when it is read again after
an LZMA2 chunk with an invalid properties byte. Fix: 7z entry readers, codec decoders and zip
entry readers are now *fused*, so after their first error, later reads repeat it.

### Archive benchmark

```powershell
cargo build -p forge-bench --release --bin archive-bench
target\release\archive-bench.exe --samples 3 --out bench.md   # --scale 0.01 for a smoke run
```

It writes a synthetic corpus once, from a fixed seed, to `<root>\zxraw-bench-corpus`
(default root `%TEMP%\zxraw-archive-bench`). It writes and deletes only inside that
folder, and it refuses to use an existing one that lacks the ownership marker it writes
when it creates it. The corpus has: text,
structured binary, already compressed (random .jpg/.mp4) and 10,000 small files. It times
create and extract for zip Fast/Balanced/Max/Zstd, 7z and tar.zst, and compares with 7-Zip
when `7z.exe` is installed. Results are in [BENCHMARKS.md](BENCHMARKS.md#archives).

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

### UI test hook isolation

In debug builds or builds with the `automation` feature, any run with `ZXRAW_SCREENSHOT`,
`ZXRAW_CMDS`, `ZXRAW_SELECT` or `ZXRAW_SCRIPT` set uses a unique throwaway profile (settings,
session, library database, caches) so concurrent automated runs do not share data. Set
`ZXRAW_PROFILE_DIR` to choose the profile explicitly. Shipped release builds ignore all
these automation variables and the profile override.
Commands run once the first listing finishes (or after 30 s), the capture happens ~90 frames
after the commands, and a run closes itself after 120 s at most.

## End-to-end tests

`crates/forge-app/tests/e2e/` drives the real `zxraw` window through `ZXRAW_SCRIPT` and asserts on
the filesystem and the app's own state afterwards. `main.rs` holds the runner and helpers; each
other file is one area (`core`, `ingest`, `library`, `tools`, `viewer`, `perf`, `browser`, `media`,
`related`, `sidebar`, `system`). Each test builds its own fixtures in a temp directory and uses a fresh
profile (`ZXRAW_PROFILE_DIR`), so no user settings, session, library data or Recycle Bin is
touched.

```powershell
$env:CARGO_TARGET_DIR = 'target/fr-e2e'
$env:CARGO_PROFILE_DEV_DEBUG = '0'   # each debug target is ~6 GB with debug info
cargo test -p forge-app --test e2e                  # all areas, ZXRAW_E2E_JOBS runs at once
cargo test -p forge-app --test e2e -- tools::       # one area
cargo test -p forge-app --bin zxraw
```

Each run opens a real window on a private desktop. Up to `ZXRAW_E2E_JOBS` runs go at once (see
Running tests), and the timing tests run alone. A run must finish within 75 s (`RunOpts::limit` raises it); the script has its own limit, 60 s by
default and changed with `limit <seconds>`.

### Isolation and seams

Automated runs (any of `ZXRAW_SCRIPT`, `ZXRAW_CMDS`, `ZXRAW_SELECT`, `ZXRAW_SCREENSHOT`) are fixed
as automated at startup and then:

* use a throwaway profile, and start in `<profile>/data/start` instead of the user's home folder;
* answer native file dialogs from the script (`dialog <path>` / `dialog cancel`, queued before the
  click that opens the dialog). An unexpected dialog fails the script instead of blocking;
* send Recycle Bin operations to `<profile>/data/recycle-sandbox` (`forge_win::set_recycle_sandbox`).
  Recycle commands in a script are refused unless that sandbox is active. The sandbox refuses
  items whose name starts with `not-recyclable` (`forge_win::recycle::SANDBOX_NOT_RECYCLABLE_PREFIX`),
  as the Recycle Bin refuses items too big for it, so scripts can reach the "Delete permanently
  instead?" dialog;
* turn off egui animations so frame counts are deterministic;
* skip the first-run welcome unless `ZXRAW_FIRST_RUN=1`;
* never check for updates, unless `ZXRAW_UPDATE_API` points the check at a test server
  (`http://127.0.0.1:<port>/…` also allows plain-HTTP loopback downloads), and never run a
  downloaded installer. `ZXRAW_UPDATE_INSTALLED=1` makes *Update now* behave as in an installed
  copy. Both variables exist only in debug and `automation` builds. Script verbs:
  `ui system wait-update <version>`, `ui system update-now`, `ui system assert-no-update`.

`ZXRAW_SCRIPT_SANDBOX` (set by the runner to the fixture root; default: the script's directory)
confines every path a step names. Paths through symlinks or junctions are checked against their
canonical target, and the `\?\` prefix is removed after the check. Screenshots may also go under
`ZXRAW_SCREENSHOT_DIR` (the runner uses `%TEMP%\zxfr-shots\<area>`), so they survive the fixture.

Fault injection (`forge-core/src/fault.rs`, armed only by `ui ingest` verbs) makes a source read
fail with `ERROR_DEVICE_NOT_CONNECTED` after N bytes, or corrupts a copy before verification, to
simulate a card pulled mid-copy.

### Script steps

A script is one step per line; `#` starts a comment. Steps run one per frame after all panes have
loaded. Waiting steps give up after 30 s unless given their own limit.

| Step | Meaning |
|---|---|
| `navigate <path>` | Open the path in the focused pane |
| `select <a>, <b>` / `select-all` | Select entries by name (comma-separated) or everything |
| `focus left\|right` | Move focus to a pane of the active tab |
| `cmd <CmdVariant>` | Run a `Cmd` by its debug name, e.g. `NewFolder` |
| `type <text>` | Replace the text of the active Prompt dialog |
| `accept` / `cancel` | Accept or dismiss the active dialog (batch rename included) |
| `conflict keep-both\|skip\|replace\|replace-if-newer\|cancel [all]` | Answer the oldest file conflict |
| `key [ctrl+][shift+][alt+]<Key>` | Send a real key press and release through egui input (`key 3`, `key ctrl+A`, `key ArrowRight`) |
| `text <text>` | Send text input to the focused widget |
| `pointer <x> <y>` / `click <x> <y>` / `scroll <dy>` | Move, click or scroll the real pointer |
| `click-entry <name>` / `double-click-entry <name>` / `right-click-entry <name>` | Real pointer click(s) where the view drew that row or tile (points, so any display scale) |
| `click-empty` | Real click on the file list below its last row or tile |
| `assert-selected <n>` | Number of selected entries the focused pane lists |
| `dialog <path>\|cancel` | Answer the next native file dialog |
| `wait-jobs [seconds]` / `wait-frames <n>` / `sleep <ms>` | Wait for operations, frames or time |
| `wait-exists\|wait-missing <path> [\| <seconds>]` | Wait for the filesystem |
| `wait-entry\|wait-no-entry <name> [\| <seconds>]` | Wait for the focused pane's listing |
| `limit <seconds>` | Raise the script limit (1–600) |
| `screenshot <png>` | Save the rendered frame |
| `assert-tabs <n>` / `assert-path <path>` | Check the tab count or the focused pane's folder |
| `assert-exists <path>` / `assert-missing <path>` | Check the filesystem |
| `assert-same <a> <b>` / `assert-contains <file> <text>` | Compare file contents |
| `assert-entry\|assert-no-entry <name>` | Check the focused pane's listing |
| `assert-toast <text>` / `assert-no-error-toast` | Check toasts raised during the run |
| `assert-notice crash\|welcome\|none` / `dismiss-notices` | Startup notices |
| `ui <area> <verb> …` | Area-specific steps, see below |
| `exit` | Stop early (optional) |

Area verbs live next to the code they drive, in `crates/forge-app/src/testhook/<area>.rs`:
`viewer` (open, cull, decode and thumbnail state), `ingest` (card fixtures, fault injection,
verification report), `library` (index, search, tags, collections, resume), `tools` (zip,
split/join, duplicates, compare/sync, external filesystem changes), `perf` (frame timing),
`browser`, `media`, `related`, `sidebar` and `system`. Each file's `step` function documents its verbs.
`ui system wait-tabs <n> [| seconds]` waits until the window has `n` tabs. The hand-off e2e
(`system::a_second_launch_opens_its_folder_in_a_new_tab_of_the_running_window`) uses it; the unit and
IPC tests are in `forge-win` `single_instance` (`cargo test -p forge-win single_instance`).

`ui perf begin` starts recording per-frame work time (from the raw-input hook to the end of the
frame) and frame interval; `ui perf end <json>` writes count, mean, p50, p95, p99 and max for both.
The report goes inside the sandbox or under `ZXRAW_SCREENSHOT_DIR`. The `perf::` e2e scenarios
record a 50,000-file folder (details and grid scrolling, keyboard paging) and 5,000 burst photos
(grouping analysis, grouped scrolling, a 1,000-file selection), and fail when p95 frame work
exceeds 100 ms in a debug build. Release-build numbers are in [BENCHMARKS.md](BENCHMARKS.md).

Prefer `click-entry` over `select` when a scenario is about what the user does: `select` sets the
selection directly and would not have caught a widget layered over the rows (a double-click
regression at 125 % display scaling was found by a user, not by the suite).

The outcome goes to `ZXRAW_SCRIPT_RESULT` (default `<script>.result`): `OK`, or `SCRIPT ERROR: …` with
exit code 2. Release builds use the Windows subsystem and have no console, so read the result file
rather than stderr when running a release binary.

A failing scenario that exposes a real application bug stays in the suite and is marked
`#[ignore = "bug: …"]` until the bug is fixed.

### Tests that touch the real system (opt-in)

`forge-win`'s `recycle_removes_file_from_disk` sends a temporary file to the user's real
Recycle Bin, so it is `#[ignore]`d by default. Run it explicitly with
`cargo test -p forge-win -- --ignored recycle_removes_file_from_disk`.
All other tests (including the e2e suite) use temporary directories and isolated profiles.

`cargo run -p forge-win --example recycle_bench --release -- [files] [runs] [mode]` times
recycling a generated tree under `%TEMP%` with the plain shell, the shell with its progress
window, `SHFileOperation` and `forge_win::recycle_items`. It uses the real Recycle Bin and
removes what it put there afterwards.
