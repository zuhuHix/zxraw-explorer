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
| Archives | `forge-core::archive`: an `ArchiveBackend` trait (list / stream entries / create / update) with the `zip` crate as the first backend (Deflate via zlib-rs and Zopfli, Deflate64, bzip2, LZMA, XZ, Zstandard, PPMd, AES and ZipCrypto); safety checks, conflicts, progress and Mark-of-the-Web are shared by all formats | 7z, tar and RAR are later backends. |
| Windows shell | `windows` crate 0.62 | Recycle Bin and shell deletes (`IFileOperation` with a progress sink), ShellExecute/"Open with", drive enumeration, Properties dialog. |

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
   that states the count and that it cannot be undone. An item the Recycle Bin refuses (too
   big, or no Recycle Bin on its drive) is never deleted permanently without that question.
5. Ingest never modifies the source; verification re-reads the *destination* from disk.
6. Archive extraction validates every entry name before writing (absolute, `..`, drive/UNC/device
   prefixes, `:` streams, NUL, reserved device names), never creates links or writes through a
   junction (the destination folders are held open by handle, unswappable, while an entry is
   written; data shorter than declared is not published), enforces entry count, size, ratio and decoder memory limits while streaming, rejects
   overlapping entries and local/central header mismatches, and copies the archive's
   Mark-of-the-Web to what it extracts.
7. External tools are launched with argument vectors, never through a shell string.
8. Metadata edits (ratings/labels/tags) are stored in the DB and/or XMP sidecars; original
   media files are not rewritten.

## Platform layer on Linux

`forge-win` keeps one public API on every platform. On Linux each module has a real backend
behind `#[cfg(target_os = "linux")]` (or `unix` where plain POSIX is enough); the Windows code is
unchanged. Large backends live next to their module (`recycle_linux.rs`, `clipboard_linux.rs`,
`drives_linux.rs`). Desktop integration goes through standard tools and protocols, not toolkit
bindings: `gio`, `gdbus`/`dbus-send`, `xdg-open` and the XDG portal are run directly, never
through a shell, with timeouts.

| Module | Linux backend |
|---|---|
| `recycle` | freedesktop.org Trash spec 1.0, written in-house: home trash (`$XDG_DATA_HOME/Trash`), `$topdir/.Trash/$uid` (sticky, not a link) or `$topdir/.Trash-$uid` on other mounts, exclusive `.trashinfo` first then `renameat2(RENAME_NOREPLACE)`, `directorysizes` kept. Recycle only renames; an item with no usable trash is `NotRecyclable`, never copied or destroyed. Permanent delete moves each item into a fresh 0700 staging folder beside it (`mkdirat`, then `renameat`), checks the confirmed identity there, and deletes only that staged object through `openat(O_NOFOLLOW)`/`unlinkat`, never crossing into a mount (mount table, `st_dev`, `STATX_ATTR_MOUNT_ROOT`); anything left is moved back with `RENAME_NOREPLACE`. Same report, progress (`TELL_EVERY`) and cancel semantics as Windows. |
| `shell` | open: `gio open`, then `xdg-open`. Reveal and properties: D-Bus `org.freedesktop.FileManager1` `ShowItems` / `ShowItemProperties`; reveal falls back to opening the parent. Open with: the XDG portal `OpenURI` with `ask=true`, falling back to the default app. Known folders: `user-dirs.dirs`. |
| `drives`, `watch`, `files` | Mounts from `/proc/self/mountinfo` (pseudo and system mounts filtered), kind from `/sys/dev/block` (`removable`, USB), labels from `/dev/disk/by-label`, space from `statvfs` on worker threads bounded by `QUERY_TIMEOUT`. `DriveWatcher` polls the mount set. File id = (folded `st_dev`, `st_ino`), volume serial = folded `st_dev`. |
| `process`, `restart` | `Command` without a shell, own process group, reaped in the background. File holders: one scan of `/proc/*/fd`, matched by device/inode or path. |
| `single_instance` | Unix domain socket in `$XDG_RUNTIME_DIR` (or a checked 0700 `/tmp/zxraw-explorer-<uid>`), same request/reply/commit protocol, timeouts and limits. Peers are checked with `SO_PEERCRED` (same uid) and `/proc/<pid>/exe` (same binary). |
| `http` | `ureq` + rustls (bundled Mozilla roots), redirects followed one hop at a time through the same URL rules as WinHTTP, proxy from `HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`. |
| `clipboard` | X11 through `x11rb` (no libxcb): an unmapped owner window and one thread serving `CLIPBOARD` requests (INCR for large values). Wayland through `wl-clipboard-rs` (data-control protocol), falling back to XWayland. Offers `x-special/gnome-copied-files`, `text/uri-list`, `application/x-kde-cutselection`, text, and our owner token. |
| `network` | `gio mount smb://host/share` (60 s limit). |
| `window` | `uncloak` is a no-op; `maximize_hidden` stays unsupported (the app maximizes through egui). |
| `drop_target`, `dragout` | X11 drag and drop in `xdnd` (`xdnd_linux.rs`): `drop_target::register` is an XDND target behind an `XdndProxy` window; `xdnd::start_drag_out` is a grab-less XDND source on its own thread (the Windows `drag_out*` functions stay `Unsupported`). No Wayland drag and drop (XWayland setting). |
| `virtual_files`, `desktop` | Unsupported (see LIMITATIONS). |
| Registry/Explorer modules | `archive_menu`, `file_types`, `user_choice`, `integration`, `shell_verb_server`, `ext_search`, `shell_notify`: stubs; Windows-only helpers are `cfg`-gated. |

`forge-media`'s `wic` module decodes HEIF/HEIC/AVIF on Linux through `libheif-rs` when the
`heif` cargo feature is on (off by default; needs libheif ≥ 1.17 development files, found with
pkg-config, plus its aom/dav1d plugins for AVIF). Without it the stubs return `Unsupported`.
