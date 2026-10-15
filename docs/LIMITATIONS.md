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
  Microsoft HEIF/HEVC (and AV1) extensions; when no installed decoder accepts a file, the
  thumbnail and viewer show an error naming the extension that may be missing. WIC decoding was tested with fixtures encoded on this machine,
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

* "Open terminal here" in Command Prompt cannot open a folder whose name contains `%` as
  administrator or on a network (UNC) path; choose PowerShell instead.
* Git status badges are off by default (Settings › Browsing) until the link-safe scan has
  passed security review.
* Git status badges never run filters or line-ending conversion, so a same-size file that Git
  stores converted (for example a Git LFS file, or `autocrlf` content) can show as modified
  after its time changes, as can same-size changes beyond the first 256 or in files over
  16 MB, and online-only files. A tracked path that is (or is below) a symlink or junction is
  never followed and shows as modified; a symlink Git itself tracks shows clean.
* Repositories show no badges when they take over 4 s, have an index over 100,000 entries or
  64 MB, need over 50,000 folders and files walked, or exceed the caps on `.gitignore` files
  (256 KB and 4,096 patterns each, 4 MB and 20,000 patterns together), `packed-refs` (16 MB),
  packs (256 packs, 256 MB of indexes, 16 GB per pack), objects (32 MB each, 512 MB
  inflated and 512 MB compressed in all) or staged changes (200,000, or 64 MB of paths). Nor
  do they on a share or mapped network drive, below a junction or symlink anywhere in the
  folder's own path (it is reached from the drive's root without following links), inside
  OneDrive or another cloud-synced folder, nor submodules and linked worktrees (a
  `.git` file could name any place, so `.git` must be a real folder), nor repositories with a
  link or a hard-linked file anywhere among the Git files read (so clones made from a local
  path, whose objects Git hard-links, show none), a work tree or `core.excludesFile` outside the
  folder, any `objects/info/alternates` entry (so `git clone --shared` clones show none), a
  split index, or SHA-256 objects. A shallow or partial clone missing `HEAD`'s objects shows
  no staged changes. A repository nested inside another, untracked one is shown as a plain
  new folder. Online-only `.gitignore` files inside a repository are skipped, not downloaded.
* The Git scan opens nothing by path but the root of a local drive; everything else is reached
  one name at a time without following links. A hard-linked tracked file whose time changed
  counts as modified (it is never read). The symlink-to-share tests need developer mode and
  were skipped on the development PC; junctions are tested.
* Files and folders can be dropped into the app. Dragging files past the window edge starts a
  Windows shell drag (`forge-win/src/dragout.rs`) that offers copy and link only, never move.
  The data object and the trigger are unit tested, but a drop into Explorer or another
  application has not been verified: it is modal and cannot be automated here. The drag starts
  only at the window edge, so the drop target must be outside ZXraw. At most 500 items can be
  dragged out at once (each costs a shell parse on the UI thread); larger selections are
  refused with a message pointing to copy and paste.
* The Windows file clipboard (`CF_HDROP`, preferred drop effect, text paths) passes its engine
  tests in `forge-win/src/clipboard.rs`. Interoperability with Explorer, and paste into or out
  of other applications, has not been checked.
* Opening a file (double-click, Enter, or **Open**) starts it in the file's own folder, as
  Explorer does. Scripts in a UNC folder (`\\server\share`) still run in the Windows folder,
  because `cmd.exe` cannot use a UNC path as its working directory; Explorer behaves the same.
  **Open With** goes through the Windows dialog, which does not take a working directory, so a
  program chosen there starts in ZXraw's own working directory.
* A queued move checks that each source is still the item that was picked, using its Windows
  file ID (NTFS, ReFS, exFAT/FAT). Only if the ID cannot be read does it compare size, modified
  time and kind instead, and that fallback misses a same-size replacement that keeps the old
  modified time. The IDs are read when the move is queued, by two shared background workers
  for local paths and two for network paths, with queued moves taking turns. If reading one
  source takes more than 10 seconds (an unreachable share), the move fails without moving
  anything and names that source; time spent waiting behind other moves does not count.
* Shell operations are Windows-only. Their non-Windows implementations return unsupported results
  or empty data. Open With, Properties, and Explorer reveal are not tested by any automated test.
* Automated runs (UI test hook, e2e suite) use a throwaway profile and recycle into a sandbox
  folder inside it. The real Recycle Bin is only exercised by an opt-in unit test
  (`forge-win` `shell::tests`, run with `--ignored`) and manual use.

* Online-only cloud files (OneDrive, Dropbox, Google Drive placeholders) are detected from
  their attributes and never read by background work, so they show a cloud badge and no
  thumbnail or metadata, even when Windows has a cached shell thumbnail. Workers re-check the
  attributes just before reading. If the attributes cannot be read, the file is treated as
  online-only. Detection has only been tested with simulated placeholders, not a live sync
  client. A file downloaded outside ZXraw keeps its badge until the folder is refreshed.
* Online-only *folders* are still listed by the indexer, scans and file operations. Listing a
  placeholder folder fetches names and sizes from the cloud, not file contents. Skipping them
  would hide whole OneDrive trees.
* The copy/move download warning checks each selected file (up to 100,000 files or 3 s). A
  selected folder is walked only if it may hold cloud files: it is inside a sync root (it or
  an ancestor carries the Cloud Files tag or online-only attribute, or that cannot be read), or
  a sync root registered with Windows (or named by the `OneDrive` environment variables) lies
  inside it, or the registrations cannot be read. A folder positively outside every cloud
  location, such as a local shoot, is not walked, so its size never causes a prompt. Before
  skipping a folder, the first 256 entries of its listing are sampled: an online-only entry
  there (an offline file on a tiered share such as Azure File Sync) makes it walked too, but
  offline files only deeper down in such a folder are missed. A sync root spelled differently
  from the selected folder (a `subst` drive, `\\localhost\c$`, a short 8.3 name) is not
  recognised as lying inside it. A walk
  stops at 20,000 entries or 300 ms; a check that could not cover everything (a limit hit, a
  folder or item it could not read) always asks: with online-only files found it gives their
  count and size as "at least", with none found it says not every file could be checked. A
  sync root created by a provider that skips the Windows shell registration is seen only from
  inside it (its folder's tag), not when a folder above it is copied. The check runs off the
  UI thread; a "Checking for online-only files" dialog appears if it takes longer than 150 ms,
  and closing that dialog stops the check. While a confirmation dialog (such as Delete
  permanently or that check) is open, a new copy or move (including a paste, a drop, or a move
  within one drive) is refused with a message rather than queued; start it again afterwards.
  Conflict questions and tool windows do not block it.
* The checksum tool counts online-only files before hashing and asks whether to download and
  hash them, skip them, or cancel. That count walks the whole selection with no limit, as
  hashing does. Skipped files are listed, never hashed, and are not in the saved manifest. A
  manifest picked for verification that is itself online-only is not read until the user
  agrees to download it; Skip stops the verification.
* Proxy generation re-checks each clip just before ffmpeg opens it and skips one that has
  become online-only since the estimate.
* Folder compare (with content hashing) and its sync preview mark online-only files "not
  compared" and leave them out of the sync. Download them first to compare or sync them.
* Hashing under *Skip online-only* and the video panel's ffmpeg work open the file first,
  check the attributes on the open handle, and keep the handle open while reading (or while
  ffmpeg runs). An open does not download a OneDrive-style (`RECALL_ON_DATA_ACCESS`)
  placeholder, and the sync client cannot dehydrate a file that has an open handle
  (`ERROR_CLOUD_FILE_IN_USE`), so those reads cannot download anything. Both were measured on
  2026-10-15 with a test Cloud Files provider (`CfRegisterSyncRoot`, no live OneDrive on that
  PC). `FILE_FLAG_OPEN_NO_RECALL` does **not** help: the Cloud Files filter ignored it and
  fully hydrated the placeholder on the first read. Remaining gaps: a provider that marks
  files `RECALL_ON_OPEN` hydrates on the open itself, so a file that becomes such a
  placeholder between the find-data check and the open is downloaded; and other background
  readers (thumbnails, metadata, duplicate scans) still check the attributes and then open
  the file separately.

## Default apps and folder opening

* After the user clicks **Make ZXraw Explorer my default**, the app registers its photo ProgIDs and
  writes each selected per-user `UserChoice` using the legacy hash format observed on Windows 11
  build 26200. Microsoft does not document this format. A Windows update, the newer
  `UserChoiceLatest` mechanism, or UCPD protection may reject a type; the UI lists failed types and
  offers ZXraw Explorer's Default apps page as a fallback for those types.
* The optional folder choice changes only `HKCU\Software\Classes\Directory\shell` and
  `Drive\shell`; it leaves `Folder\shell` alone so virtual folders keep their Windows behavior.
  Win+E, the taskbar File Explorer icon, and other apps' **Show in folder** actions that call
  `SHOpenFolderAndSelectItems` still open Windows Explorer.
* Apps that launch a specific program themselves (for example an **Open in Photos** button) are not
  affected. Applications that ask the Windows shell to open a photo or filesystem folder use the
  selected defaults.
* Passing a folder or drive to ZXraw opens it in a tab in that process. ZXraw has no single-instance
  IPC, so opening it from another app can start another window. Each double-clicked photo also starts
  a new window.

## Installer, packaging, and licences

* The installer (`packaging/zxraw.iss`, Inno Setup 6.7.3) was compiled from `ed5fc58`. A silent
  install into a temporary folder, a launch of the installed app and a silent uninstall all
  succeeded. The interactive wizard and the optional context-menu task were not exercised.
  Neither the installer nor the executables are code-signed, so SmartScreen warns on first run.
* The release zip was built from `ed5fc58` and its executable launched. The release script builds with
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
  depending on the build) apply and must be shipped. **Download FFmpeg…** fetches a GPL v3 build
  from its publisher at the user's request and installs its licence alongside it; ZXraw Explorer
  does not redistribute it.
* The update check asks `api.github.com` once a day, unauthenticated. GitHub allows 60 such
  requests an hour per IP address, so on a shared network the check can be refused; it then
  stays silent and tries again the next day (**Check now** shows the error). The installer is
  checked against `SHA256SUMS.txt` from the same GitHub release, which protects against a
  corrupted or swapped download but not against someone who controls the release itself; the
  installer is not code-signed.
* An in-app update closes the app and runs the installer silently. If another ZXraw Explorer
  window is still open after 30 seconds, the installer stops without changing anything, and
  the app has to be started again by hand.
* The FFmpeg download is pinned to one build (9.0.2). A newer FFmpeg needs a new app release
  (or installing it yourself). If the publisher removes that release, the download fails with
  an HTTP error and nothing is installed.

## Organising and metadata

* Burst and similar-photo grouping works in the grid view only (not details). It uses
  capture time (EXIF, else modification time) and a 64-bit difference hash (dHash) of the thumbnail, so heavily
  edited versions or crops of the same shot are not grouped, and unrelated photos with very
  similar composition can be. Hidden files take part in the analysis (so a filter change needs no
  re-run), so a hidden shot can link two visible ones into one stack; stacks only ever select,
  drag or tag the files the listing shows.
* The GPS panel is offline; "Open in maps" opens the configured http/https URL in the
  default browser only when clicked. There is no embedded map and no reverse geocoding.
* Metadata export needs the library database to be open (it is opened at startup), and
  video fields stay empty without `ffprobe`.

## Folder compare and sync

* Sync re-checks every planned action against the preview before acting. A destination it would
  overwrite is guarded by a full BLAKE3 hash, checked again right before the finished copy is
  published; only the instant between that check and the rename is unguarded. Sources and
  right-only files that Mirror recycles are guarded by size, modification time, and a sampled
  hash above 1 MiB (five 64 KiB blocks), so an edit between the samples with the timestamp put
  back is not noticed: the newer source bytes are copied, or the edited file is recycled
  (recoverable from the Recycle Bin).
* A Replace chosen in a copy or move conflict, and split/join rollback, check the destination's
  kind, size and modification time right before acting, not its content or Windows file ID: a
  same-size replacement that keeps the exact timestamp would not be noticed.
* Mirror recycles a deleted folder's previewed files one by one and then removes the folders
  only if they are empty, so a file created inside during the sync keeps its folder (reported
  as skipped). The Recycle Bin then holds the files individually rather than the folder.
* Copies and deletes check the last path component for links and junctions. A junction swapped
  into the middle of a path while a sync runs is followed.
* When Windows cannot recycle an item (for example on some network drives), the shell asks
  whether to delete it permanently. Answering yes deletes it outside the preview.

## Find and replace in files

* Protection against a concurrent edit is best-effort, with a window of microseconds. While a
  file is read, checked and backed up, other programs cannot write to it. The deny-write handle
  must be closed before `ReplaceFileW` can run, so the size, last-write time and file ID are
  compared again as the very last step before the swap. A writer that gets in between that
  comparison and `ReplaceFileW` is not detected: its edit is replaced. The backup in the profile
  still holds the bytes that were read. Undo works the same way.
* Junction and link checks run on every folder between the chosen root and the file, before the
  file is read and again right before the swap (apply and undo). A junction swapped in during the
  same microsecond window is not detected.
* Hard-linked files (more than one link) are always skipped; there is no option to replace them.
* Only known text file types are searched unless *Include files* names the type. An include
  pattern of `*` therefore opts every type in, although known binary types are still skipped.

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
* A WIC decode (HEIC/AVIF) runs inside the Windows codec and cannot be interrupted. If a codec
  or a network-backed file stalls, the thumbnail worker waits, and closing the app waits for it.
* Export-association previews are scanned by a single worker; a scan stuck on an unreachable
  network folder delays the previews queued behind it.
* Network locations: there is no browsing of `\\` (no SMB discovery of servers or shares), no
  mapping of drive letters, and no stored credentials; sign-in is Windows' own dialog. A
  status check that has not answered after 10 s (running, or still queued behind other
  checks) shows the location as offline; the check itself keeps running in the OS until
  Windows gives up (often 20–60 s on an unreachable host), and it holds one of the probe
  service's 3 network workers meanwhile.
* Deciding whether a drive letter is a network drive uses `GetDriveTypeW`. It answers from
  Windows' local device table in practice, but Windows does not document that it never
  touches the network. UNC paths are decided from the text alone.
* *Connect…* on a mapped drive connects the drive's share without a letter; Windows then
  reconnects the letter on its next use. A share already connected as another user fails
  with Windows' "multiple connections" error until that connection is closed.
* Only one *Connect…* runs at a time (one Windows sign-in dialog); asking for another share
  meanwhile says to finish the first. A sign-in dialog that is still open when the app exits
  is not cancelled by the app: it stays until the user closes it.
* A network path typed in the address bar (or given on the command line) is not checked on
  the UI thread: it is opened as a folder, and if it turns out to be a file its folder opens
  with the file selected once the background listing finds out. On the command line a
  photo, video or audio extension is taken to mean a file. The address bar offers no folder
  completions for network paths.
* Network thumbnail and hashing work have no per-file timeout: a file that stalls on the
  server holds one of the 2 network thumbnail workers (or the single full-hash worker)
  until the OS call returns. Local folders are not affected because they use separate
  workers.
* Clicking a network location whose first check is still running opens it; the folder listing
  then runs in the background and shows its own error if the share does not answer.
* The copy path keeps the fault-injection reader used by the e2e "card pulled" tests in release
  builds. When not armed it costs one relaxed atomic load per read chunk.

## Metadata and XMP sync

* XMP sidecar sync is opt-in. When it is enabled, ratings, labels, keywords, and (for photos and
  RAW files only) notes are written. Notes go to `xmpDM:logComment`, chosen so the caption in
  `dc:description` is never overwritten; Lightroom does not display that field. Picks stay in the
  app library (`library.rs`). Notes already present in a sidecar are not read back into the library.
* Notes and ratings follow renames and moves made inside ZXraw. A file renamed or moved by another
  program keeps its note on the old path until it is relinked (Collections › Relink).
* A file that appears at a path that previously held a different, externally moved file inherits
  its note, tags and rating.
* A sidecar saved as UTF-16 is never rewritten. The sync error says "sidecar is UTF-16", and the
  note (or rating, label, keywords) is saved in the library only.
* A note the database had not confirmed when ZXraw closed is kept in `pending-notes.json` in the
  app data folder and saved to the library at the next start. That replay does not update the
  XMP sidecar; the next edit of the note does. A recovery file that cannot be read is renamed to
  `pending-notes.json.unreadable-<timestamp>` and left for manual recovery; it is not replayed.
* The sidecar writer is tested for lossless merges, namespaces, and same-process serialisation,
  but it has not been tested with Lightroom, Capture One, or darktable.
* Updates within ZXraw are serialised. Against other programs, the final compare and replace run
  while ZXraw holds a handle that denies other writers, and a sidecar another program has open
  for writing is reported as a conflict instead of overwritten. A program that replaces the
  sidecar by renaming a new file over it in that same instant is not excluded.
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
* Frame times for a 50,000-file folder (scrolling details and grid, keyboard paging) were measured
  in a release build on one machine; see [BENCHMARKS.md](BENCHMARKS.md). Listing, thumbnail decoding,
  grouping analysis on large real photo sets, and network folders were not part of that measurement.

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
