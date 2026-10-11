# Feature checklist

Checked against the current working tree (commit `476021b` plus uncommitted changes) on
2026-10-10/11.

Status means:

* ✅ a test, benchmark, or live check in this checkout passed for the stated path.
* 🟡 source exists, but the complete workflow or user-visible behaviour is partial,
  unverified, or still changing (including "landed, verification in progress").
* ⬜ no implementation was found in the current source.

A passing engine test does not prove visual correctness or Windows shell interoperability.
The exact commands and limits are in [TESTING.md](TESTING.md) and
[BENCHMARKS.md](BENCHMARKS.md).

## Navigation

* 🟡 Browse a directory by path, open parent folders, and keep back/forward history —
  `forge-app/src/pane.rs`. Only the 50k-folder engine listing is measured; the rendered
  navigation path is not automated.
* 🟡 Tabs, dual panes, session restore, address/breadcrumb navigation, and drive sidebar —
  `forge-app/src/{app,pane,breadcrumb,sidebar}.rs`. Interactive flows are not automated.
* ✅ Stream directory entries in batches (engine) — `forge-core/src/listing.rs`
  (`list_dir_streaming`). The 50,000-entry flat-directory run in
  [BENCHMARKS.md](BENCHMARKS.md) completed with zero listing errors.
* 🟡 Cancel a superseded listing in the UI — the cancel token is wired in
  `forge-app/src/pane.rs`; cancellation in the rendered app is not exercised.
* 🟡 Browse a 50,000-file folder in the desktop UI — the screenshot hook loads the folder
  and captures a frame; see the scenario result in [BENCHMARKS.md](BENCHMARKS.md). Frame
  pacing and interactive responsiveness are not measured.

## File operations

* 🟡 Copy, move, create-folder, and delete with conflict handling — engine code and unit
  tests in `forge-core/src/ops.rs`; the UI path is not automated.
* 🟡 Recycle Bin delete, open files, Open With, Properties, and reveal in Explorer —
  `forge-win/src/shell.rs`. Windows shell behaviour is not exercised by any test here.
* 🟡 Copy/cut/paste through the Windows clipboard — `forge-win/src/clipboard.rs` publishes
  and reads `CF_HDROP`, preferred drop effect, and text paths; `forge-win` has 22 unit tests.
  Explorer interoperability has not been checked.
* 🟡 Permanent-delete confirmation, operation queue, pause/cancel, and report UI —
  `forge-app/src/ops_ui.rs` and `forge-jobs`. Visual workflows are not automated.

## Power-user tools

* ✅ Batch rename preview and apply, with collision and problem checks — engine in
  `forge-core/src/rename.rs`; CLI tests `rename_dry_run_changes_nothing_and_apply_renames`
  and `rename_refuses_on_problems` in `forge-cli/tests/cli.rs`.
* ✅ BLAKE3 hashing and verification — `hash_verify_roundtrip_detects_corruption` and
  `hash_prints_b3sum_style_lines_without_manifest` in `forge-cli/tests/cli.rs`; full-file
  throughput is measured in [BENCHMARKS.md](BENCHMARKS.md).
* ✅ Duplicate detection — `dupes_finds_pair_and_deletes_nothing` in `forge-cli/tests/cli.rs`;
  the 200k-file benchmark tree yields the expected 20 duplicate groups with zero errors.
* 🟡 Folder size analysis — `forge-core/src/disk_usage.rs`; no end-to-end or benchmark
  result is recorded.
* 🟡 Compare folders/files, keyboard command palette, and dual-pane workflows — engine and
  UI code exist (`forge-core/src/compare.rs`, `forge-app/src/palette.rs`); no interaction
  path is automated.

## Photography browsing, culling, metadata, and integrations

* 🟡 JPEG/PNG/TIFF/WebP/GIF/BMP thumbnail generation and cache — `forge-media` tests cover
  formats and cache invalidation. The benchmark generates 256 px thumbnails from 200 x
  24 MP JPEGs (cold and disk-cache passes, [BENCHMARKS.md](BENCHMARKS.md)); the PNG, TIFF,
  WebP, GIF, and BMP paths are not benchmarked.
* 🟡 EXIF/XMP reading, image orientation, and embedded RAW JPEG preview extraction —
  `forge-media` integration tests use synthetic JPEG/TIFF/CR3/RAF/ORF/RW2 data. This is not
  validation against real camera files from every vendor.
* 🟡 Ratings, pick state, labels, tags, and collections persisted in SQLite — storage in
  `forge-db` has tests; the annotation UI is not automated.
* 🟡 Culling/compare viewer, zoom/pan, and filmstrip — `forge-app/src/viewer/` (cull, zoom,
  loader, canvas) and `viewer_host.rs`. Unit tests exist for culling and zoom; no visual
  interaction is certified.
* 🟡 XMP sidecar and Windows shell integrations — parser and storage code exists; external
  application interoperability was not exercised.

## Video

* 🟡 Parse `ffprobe` metadata and derive representative frames with external tools —
  `forge-media/src/video.rs`. Requires local FFmpeg executables, which are not bundled or
  exercised here.
* ⬜ In-app video playback — not implemented. The UI states "video playback is out of
  scope" (`forge-app/src/video_ui.rs`); videos open in an external application.
* 🟡 Filmstrip/scrubbing, related clips, and proxy generation — `forge-app/src/video_ui.rs`
  (landed, verification in progress). Its unit tests exist but the workflow is not
  certified.

## Ingest

* ✅ Plan an import, apply verified copies, and leave the source intact — CLI test
  `ingest_dry_run_copies_nothing_apply_copies_and_verifies` in `forge-cli/tests/cli.rs`.
* 🟡 Ingest UI, template editing, progress/report display, and cancellation —
  `forge-app/src/ingest_ui.rs`. The UI workflow is not automated.

## Search

* ✅ Evaluate structured filters over listing entries — the benchmark runs
  `Query::matches_entry` over 50,000 entries (12,500 matches for `ext:jpg OR ext:cr3`) in
  [BENCHMARKS.md](BENCHMARKS.md). Parser unit tests in `forge-core/src/query.rs` were not
  re-run in this pass.
* ✅ Index folders in SQLite and search indexed metadata — `forge-db` indexer/search tests,
  CLI test `search_after_index_finds_file`, and the 200,000-file index/search benchmark.
* 🟡 Resume interrupted scans — `forge-db` indexer tests exist; interrupted-scan recovery is
  not benchmarked or exercised end to end.
* 🟡 Saved searches and exclusions — database paths exist and the collections UI exposes
  saved searches (`forge-app/src/collections_ui.rs`, landed, verification in progress).

## Library and collections

* 🟡 Store index roots, offline rows, ratings/tags, collections, and saved searches in the
  local database — `forge-db/src/collections.rs` includes export/import round-trip tests
  (landed, verification in progress).
* 🟡 Library/collection browser, editing, backup/relink, and saved-search management —
  `forge-app/src/collections_ui.rs` (landed, verification in progress). User interaction is
  not verified.

## Archives and extra tools

* 🟡 Browse, extract, and create ZIP archives; create contact sheets; split and join
  files — `forge-core/src/{archive,split}.rs` and `forge-app/src/extra_tools_ui.rs`
  (landed, verification in progress). 7z and RAR are not implemented.

## Performance

* ✅ Streaming directory listing in the engine (`list_dir_streaming`) — 50,000-entry runs
  in [BENCHMARKS.md](BENCHMARKS.md).
* 🟡 Listing off the UI thread, and the bounded duplicate-hash worker pool (8 partial-hash
  and 4 full-hash I/O threads, `forge-core/src/duplicates.rs:168,198`) — the duplicate scan
  returns correct results in the benchmark, but thread behaviour and the UI-thread split are
  not measured.
* 🟡 Virtualised table/grid painting — `egui::show_rows` is used in `forge-app/src/views.rs`
  (details list and thumbnail grid); the frame-pacing effect is not measured.
* ✅ Thumbnail disk cache, DB indexing, and file hashing — measured by the benchmark
  harness under concurrent build load. Numbers are not regression baselines.

## UI/UX

* 🟡 egui desktop shell, list/grid modes, keyboard commands, themes, preview pane — source
  is present; visual review is not complete.
* 🟡 AccessKit accessibility nodes (landed, verification in progress) — the `eframe`
  `accesskit` feature is enabled in `Cargo.toml`, and `forge-app/src/views.rs` builds the
  nodes. No screen-reader test has been run.
* 🟡 `ZXRAW_SCREENSHOT`, `ZXRAW_CMDS`, and `ZXRAW_SELECT` smoke hook — `forge-app/src/testhook.rs`.
  It does not assert pixels or simulate pointer input.

## Reliability, privacy, and packaging

* 🟡 Recursive operations avoid following links implicitly; copies stage to a temp name;
  ZIP extraction rejects unsafe paths — engine code and tests exist; run status is recorded
  with the test results in [TESTING.md](TESTING.md).
* 🟡 SQLite migrations, WAL/foreign keys, integrity checks, and corrupt-database recovery —
  `forge-db` unit and integration tests cover these paths (`forge-db/tests/indexer.rs`).
* 🟡 Worker panics are caught and long jobs support cancellation and pause — `forge-jobs`
  has 4 unit tests; the `forge-app/src/crash.rs` crash-report path is landed and in
  verification.
* 🟡 Crash reports (landed, verification in progress) — `forge-app/src/crash.rs` writes a
  report on panic and prunes old reports; it is not triggered end to end here.
* 🟡 Packaging (landed, verification in progress) — `packaging/build-release.ps1`,
  `packaging/zxraw.iss` (Inno Setup), and `packaging/generate_icon.py`. No installer has been
  built or installed in this pass.
* 🟡 Local-first/no-telemetry direction is stated in `README.md`; no network-monitoring audit
  has been performed.

## In-progress work

The following are present in the working tree and remain 🟡 until their tests pass and
their user-facing workflows are exercised: viewer/culling, ingest UI, power-user tools,
settings and external tools, the Windows clipboard, folder watching, collections, the video
workflow, contact sheets and archive/split-join tools, accessibility nodes, and packaging
with crash reports.
