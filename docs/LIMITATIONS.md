# Known limitations

This list describes commit `dfd43d9` (branch `main`, clean working tree), checked on 2026-10-11.
It is not the intended roadmap. The app is still in early development; a feature appearing in a
menu or module is not evidence of a complete end-to-end workflow. Feature status, with the tests
that back each line, is in [FEATURES.md](FEATURES.md).

## Formats and media

* Archive support is ZIP only. There is no 7z, RAR, or TAR reader (`forge-core/src/archive.rs`).
* ZXraw does not develop RAW files. RAW preview support extracts an embedded JPEG preview;
  it does not demosaic sensor data. A RAW with no usable embedded JPEG has no image preview.
* Thumbnails use the Rust `image` pipeline for JPEG, PNG, TIFF, WebP, GIF, and BMP. HEIC/HEIF
  and AVIF go through Windows Imaging Component (`forge-media/src/wic.rs`), so they need the
  Microsoft HEIF/HEVC (and AV1) extensions; without them the thumbnail and viewer show a
  "codec not installed" error. WIC decoding was tested with fixtures encoded on this machine,
  not with camera or phone files.
* Thumbnail throughput is limited. On this machine, under concurrent build load, 256 px
  thumbnails from 24 MP JPEGs were generated at a median of 3.7 images/s with an empty disk
  cache (about 272 ms per image). Cache hits take about 1 ms. Only JPEG throughput has been
  measured. See [BENCHMARKS.md](BENCHMARKS.md).
* There is no in-app video playback: the video UI says "video playback is out of scope"
  (`forge-app/src/video_ui.rs:557`). Videos open in an external application.
* FFmpeg and ffprobe are not bundled and were not installed on the verification machine. Video
  metadata parsing and proxy argument handling are tested with canned tool output and fake tools
  only. Real proxy generation, filmstrip rendering, and scrubbing have not been run. Proxy
  placement never replaces an existing file (tested).

## Windows shell and file exchange

* Files and folders can be dropped into the app. Dragging files past the window edge starts a
  Windows shell drag (`forge-win/src/dragout.rs`) that offers copy and link only, never move.
  The data object and the trigger are unit tested, but a drop into Explorer or another
  application has not been verified: it is modal and cannot be automated here. The drag starts
  only at the window edge, so the drop target must be outside ZXraw.
* The Windows file clipboard (`CF_HDROP`, preferred drop effect, text paths) passes its engine
  tests in `forge-win/src/clipboard.rs`. Interoperability with Explorer, and paste into or out
  of other applications, has not been checked.
* Shell operations are Windows-only. Their non-Windows implementations return unsupported results
  or empty data. Open With, Properties, and Explorer reveal are not tested by any automated test.
* Automated runs (UI test hook, e2e suite) use a throwaway profile and recycle into a sandbox
  folder inside it. The real Recycle Bin is only exercised by an opt-in unit test
  (`forge-win` `shell::tests`, run with `--ignored`) and manual use.

## Installer, packaging, and licences

* The installer is not compiled. `packaging/zxraw.iss` (Inno Setup) has not been built, because
  Inno Setup (`ISCC`) is not installed on the verification machine.
* The release zip was built and its executable launched (commit `e3bfbd2`). That build was not
  repeated for this checkout, and no `dist/` output is present. The release script builds with
  `--no-default-features`, so the automation hook is compiled out. This was not checked in a
  release binary.
* Licence notices that must ship before any distribution (flagged in `packaging/THIRD_PARTY.md`):
  * `epaint_default_fonts` 0.36.2, `(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0`. The
    SIL Open Font License 1.1 and Ubuntu Font Licence 1.0 texts and the font copyright notices must
    be included with the release. The release script currently copies only `LICENSE-MIT`,
    `LICENSE-APACHE`, `README.md`, `THIRD_PARTY.md`, and `LICENSES-NOTE.md`, so this is not yet met.
  * `option-ext` 0.2.0, `MPL-2.0`. This is weak copyleft. The MPL-2.0 licence text and the
    crate's notices must ship, and the source of the crate must remain available under MPL-2.0.
    ZXraw has not modified the crate.
  * `self_cell` 1.3.0, `Apache-2.0 OR GPL-2.0-only`. ZXraw elects Apache-2.0, so the Apache-2.0
    text (already shipped as `LICENSE-APACHE`) applies. GPL-2.0 is not elected.
* FFmpeg and ffprobe are not bundled. If they are ever bundled, their own licences (LGPL or GPL,
  depending on the build) apply and must be shipped.

## Organising and metadata

* Burst and similar-photo grouping works in the grid view only (not details). It uses
  capture time (EXIF, else modification time) and a 64-bit difference hash (dHash) of the thumbnail, so heavily
  edited versions or crops of the same shot are not grouped, and unrelated photos with very
  similar composition can be. Groups are recomputed when the folder is reloaded.
* The GPS panel is offline; "Open in maps" opens the configured http/https URL in the
  default browser only when clicked. There is no embedded map and no reverse geocoding.
* Metadata export needs the library database to be open (it is opened at startup), and
  video fields stay empty without `ffprobe`.

## Scale and background work

* A pane keeps the complete `FileEntry` list and a separate visible-index list
  (`forge-app/src/pane.rs:33,35`). Natural sorting covers the whole list, and rebuilding a
  filter examines every entry. Virtualised table and grid painting (`show_rows`, in
  `views.rs` at lines 878 and 1166) bounds row widgets to the visible region. It does not make
  listing, sorting, or filtering constant-time or constant-memory.
* Measured at the median on a 50,000-entry folder (under concurrent build load): first batch
  44 ms, complete listing 414 ms, sort per resort 33 ms, filter 94 ms. Listing a 200,000-file
  tree and indexing it takes about 68 s fresh and 20 s to re-index unchanged. See
  [BENCHMARKS.md](BENCHMARKS.md).
* The duplicate scan of the 200,000-file tree took 23 s in one invocation and 805 s (median)
  and 1,662 s in two others, over the same files with the same result. The cause is not
  isolated. Duplicate scans should not be relied on for timing until this is explained. A
  full hash of the 2 GB file runs at about 560 MB/s.
* Opening freshly created small files took a median of 0.45 ms each on this machine, so the
  reported ~10 ms per cold open was not reproduced. This test does not support per-open
  antivirus cost as the explanation for the duplicate-scan spread; the duplicate scan itself
  was not profiled per stage (see [BENCHMARKS.md](BENCHMARKS.md)).
* Benchmark times depend on the local filesystem, Windows cache state, real-time antivirus,
  and concurrent load. The OS file cache is not flushed. "Cold" describes the first
  application pass over data or a missing ZXraw disk-cache entry.
* Indexing and thumbnail jobs are local. There is no cloud catalog, remote database, or
  multi-machine synchronisation.

## Metadata and XMP sync

* XMP sidecar sync is opt-in. When it is enabled, only ratings, labels, and keywords are written.
  Picks and notes stay in the app library (`library.rs`) and are not written to sidecars.
* The sidecar writer is tested for lossless merges, namespaces, and same-process serialisation,
  but it has not been tested with Lightroom, Capture One, or darktable.
* A small cross-process race window remains. Updates within ZXraw are serialised. Another process
  writing the same sidecar at the same moment is not excluded by a lock.
* EXIF and XMP reading is tested on synthetic JPEG, TIFF, PNG, WebP, CR3, RAF, and ORF data. It has
  not been validated against real camera files from every vendor.

## Accessibility

* The AccessKit nodes are checked by unit tests. No screen reader or other assistive technology
  (for example NVDA or Narrator) has been run against the app.
* The colour-contrast tests cover the selected row, hyperlinks, and the focus ring. Other colours in
  the UI are not covered by a test.

## Verification boundary

* The unit and integration suites verify the tested engine paths and UI logic. They do not verify
  every Windows shell call, screen-reader workflow, GPU rendering path, or behaviour on a real
  photo or video library.
* The e2e suite (`crates/forge-app/tests/e2e/`) drives the real window with injected egui key and
  pointer events and scripted file-dialog answers, and asserts on files and app state. It does not
  exercise the native dialogs themselves, OS-level input (IME, touch, pen), multi-monitor DPI
  changes, or real removable media: a "card" is a temp folder, and a pulled card is simulated by
  fault injection in the copy reader. Screenshots are inspected by a person, not compared pixel by
  pixel. See [TESTING.md](TESTING.md).
* The 50,000-folder screenshot in [BENCHMARKS.md](BENCHMARKS.md) shows that the list renders; it does
  not establish frame pacing or interactive responsiveness. That run predates `dfd43d9`.

## Fixed since commit 476021b

These limitations from the earlier version of this list no longer apply:

* The UI test hook ran against the user's real profile. It now uses a throwaway profile unless
  `ZXRAW_PROFILE_DIR` is set (`forge-app/src/settings.rs`), and the e2e suite uses its own profile
  (commit `25b10cd`).
* Publishing could replace an existing output. Files, archives, contact sheets, and split parts now
  publish without replacing existing files, using `rename_no_replace` (commits `91407a3`, `9968bbd`,
  `f67d83c`).
* Collection relink did not merge annotations on a path collision, and collection import conflicts
  were not reported. Relink now merges them and import reports conflicts (commit `e99b827`).
