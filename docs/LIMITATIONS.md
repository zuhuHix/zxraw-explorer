# Known limitations

This list describes the checked-out implementation (commit `476021b` plus the uncommitted
working tree), not the intended roadmap. The app is still in early development; a feature
appearing in a menu or module is not evidence of a complete end-to-end workflow. Feature
status is in [FEATURES.md](FEATURES.md).

## Formats and media

* Archive support is ZIP only. There is no 7z, RAR, or TAR reader (`forge-core/src/archive.rs`).
* ZXraw does not develop RAW files. RAW preview support extracts an embedded JPEG preview;
  it does not demosaic sensor data. A RAW with no usable embedded JPEG has no image preview.
* Thumbnails use the Rust `image` pipeline for JPEG, PNG, TIFF, WebP, GIF, and BMP. HEIC/HEIF
  and AVIF pixels are not decoded: the thumbnail path returns "not available yet"
  (`forge-media/src/thumbs.rs:781`), and only their EXIF is read (`image_meta.rs:25`). The
  WIC fallback is planned work (`thumbs.rs:710`), not present.
* Thumbnail throughput is limited. On this machine, under concurrent build load, 256 px
  thumbnails from 24 MP JPEGs were generated at a median of 3.7 images/s with an empty disk
  cache (about 272 ms per image). Cache hits take about 1 ms. See [BENCHMARKS.md](BENCHMARKS.md).
* Video metadata and thumbnails depend on external `ffprobe` and `ffmpeg` executables, which
  are not bundled. There is no in-app video playback: the video UI says "video playback is
  out of scope" (`forge-app/src/video_ui.rs`). Filmstrip, scrubbing, and proxy generation
  exist but are landed and still in verification.

## Windows shell and file exchange

* `eframe` accepts files and folders dropped into the app. There is no outgoing OLE drag
  source (`IDropSource`/`DoDragDrop` do not appear in the source), so ZXraw cannot drag files
  out to another application.
* The Windows file clipboard publishes and reads `CF_HDROP`, the preferred drop effect, and
  text paths (`forge-win/src/clipboard.rs`). Its unit tests run, but Explorer
  interoperability and the complete copy/cut/paste workflow have not been verified.
* Shell operations (Recycle Bin delete, Open With, Properties, Explorer reveal) are
  Windows-only. Their non-Windows implementations return unsupported results or empty data.
* Session restore reopens tabs from the user's real profile. The UI test hook runs against
  that profile too, so a test run shows the user's own tabs.

## Scale and background work

* A pane keeps the complete `FileEntry` list and a separate visible-index list
  (`forge-app/src/pane.rs:33,35`). Natural sorting covers the whole list, and rebuilding a
  filter examines every entry. Virtualised table and grid painting (`show_rows`,
  `views.rs:821,1109`) bounds row widgets to the visible region. It does not make listing,
  sorting, or filtering constant-time or constant-memory.
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

## Verification boundary

Unit and integration tests verify the tested engine paths, not every Windows shell call,
screen-reader workflow, GPU rendering path, or behaviour on a real photo or video library.
The UI test hook captures a screenshot after commands and does not assert pixel-level
correctness or automate pointer or keyboard input. The 50,000-folder screenshot run shows
that the list renders; it does not establish frame pacing or interactive responsiveness.
See [TESTING.md](TESTING.md) and [FEATURES.md](FEATURES.md) for current status.
