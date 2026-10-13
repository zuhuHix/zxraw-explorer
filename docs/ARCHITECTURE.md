# ZXraw Explorer — Architecture

The product is called **ZXraw Explorer**. Internal crates keep the `forge-*` prefix from the
project's working name; it never appears in the UI.

## Decisions

| Area | Choice | Why |
|---|---|---|
| Language | Rust 2024 (MSRV 1.90) | Requirement; memory safety for untrusted media parsing. |
| UI | **egui / eframe 0.36** (wgpu on DX12, AccessKit enabled) | Pure Rust, one process, no web runtime. Immediate mode makes virtualized lists/grids of 50k+ rows trivial (`show_rows`), redraws only on input/repaint requests, renders on the GPU. AccessKit gives UI Automation (screen readers). Trade-off vs Tauri: less "native" look, but no IPC/serialization boundary between the file engine and the view, and far lower memory. Native Windows dialogs/shell menus are still used via `rfd` / `forge-win`. |
| Database | SQLite via `rusqlite` (bundled) | Single-file, transactional, WAL mode, FTS-capable. |
| Hashing | BLAKE3 (verification/duplicates), xxh3 (quick partial-hash prefilter) | BLAKE3 is fast and cryptographically strong; xxh3 is cheap for staging. |
| Images | `image` crate (JPEG/PNG/TIFF/WebP/GIF/BMP) + WIC fallback (Phase 3) | Pure Rust decoders for the common path; WIC adds HEIC/AVIF/RAW codecs the user has installed. |
| RAW | Embedded JPEG previews extracted via EXIF/TIFF IFD parsing (`kamadak-exif`) | Every mainstream RAW embeds a full-size or large JPEG preview. We do not develop RAWs (explicit non-goal). |
| Video | **External `ffprobe`/`ffmpeg` executables**, discovered on PATH or configured | No linking → no LGPL/GPL redistribution obligations in our binary, crash isolation (a codec crash kills a child process, not the app), hard timeouts. ZXraw Explorer works fully without them; video metadata/thumbnails degrade gracefully. |
| Archives | `zip` crate for browse/extract/create; path traversal checked | 7z/RAR: not yet (documented limitation). |
| Windows shell | `windows` crate 0.62 | Recycle Bin (`IFileOperation`), ShellExecute/"Open with", drive enumeration, Properties dialog. |

## Crates

```
forge-app   (bin "zxraw")   egui UI, interaction state, session restore
   ├── forge-core           platform-independent FS engine
   │     model, listing, paths, ops, rename, hashing, duplicates,
   │     disk_usage, compare, query, ingest, archive
   ├── forge-db             SQLite schema/migrations, index, ratings/tags/collections, caches
   ├── forge-media          image/RAW/video metadata, thumbnail pipeline, thumbnail disk cache
   ├── forge-win            Windows-only shell integration (no-ops / errors elsewhere)
   └── forge-jobs           CancelToken, Progress, bounded priority WorkerPool, JobRegistry
forge-cli   (bin "zxraw-cli") hashing, duplicates, rename preview, ingest from the terminal
```

Dependency rule: `forge-jobs` ← `forge-core` ← (`forge-db`, `forge-media`) ← `forge-app`.
`forge-win` depends on nothing internal. Only `forge-app` knows about the UI.

## Concurrency model

* The UI thread never touches the filesystem synchronously except for trivially cheap
  calls (path existence checks on user-typed paths are done off-thread too).
* Directory listings run on a dedicated thread per request and stream batches back over a
  channel; a newer navigation cancels the older listing via `CancelToken`.
* Bounded pools (`WorkerPool`) with **priority + bounded queue**:
  * `io`      — metadata reads, hashing small files
  * `image`   — image decode / thumbnail scaling (≈ cores/2)
  * `video`   — ffmpeg child processes (2)
  When a queue is full, lower-priority work is evicted; visible thumbnails are `High`,
  prefetch is `Low`. Newest request wins within a priority (LIFO) because it is what the
  user is looking at.
* Long user-visible jobs (copy queues, indexing, ingest, duplicate scans) are registered in
  `JobRegistry` with pause/cancel and a report log.
* Panics inside workers are caught; a corrupt file cannot take down the app.

## Startup

* The window is created hidden and shown after the first frame is painted (eframe). A saved
  maximized state is applied by `startup::maximize_hidden` while the window is DWM-cloaked,
  not by winit, which would flash a blank window on screen while maximizing it.
* Graphics use wgpu on DX12 only, so the Vulkan and OpenGL drivers are not loaded at startup.
  If the DX12 setup fails before the app starts, the window is created again with eframe's
  default backends (Vulkan, DX12, OpenGL) and a warning is logged. Setting `WGPU_BACKEND`
  (`vulkan`, `gl`, `dx12`) skips the DX12-only attempt and uses eframe's own backend choice.

## Data locations

* Settings/session: `%APPDATA%\ZXraw Explorer\config\` (JSON, atomic write-rename)
* Database: `%LOCALAPPDATA%\ZXraw Explorer\data\forge.db`
* Thumbnail cache: `%LOCALAPPDATA%\ZXraw Explorer\cache\thumbs\` (keyed by path hash + size + mtime)
* Logs: `%LOCALAPPDATA%\ZXraw Explorer\data\logs\zxraw.log.*` (rotating; paths are logged at debug level only)

## Safety invariants

1. Recursive operations never follow reparse points (junctions/symlinks) implicitly.
2. Copies write to a temp name in the destination dir and rename into place, so an
   interrupted copy never leaves a truncated file under the final name.
3. Overwrites require an explicit conflict policy; the default is *ask*.
4. Delete defaults to the Recycle Bin; permanent delete requires a separate confirmation
   that states the count and that it cannot be undone.
5. Ingest never modifies the source; verification re-reads the *destination* from disk.
6. Archive extraction rejects absolute paths, `..` components, and drive prefixes.
7. External tools are launched with argument vectors, never through a shell string.
8. Metadata edits (ratings/labels/tags) are stored in the DB and/or XMP sidecars; original
   media files are not rewritten.
