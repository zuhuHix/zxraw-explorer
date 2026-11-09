# Known limitations

This list describes commit `dfd43d9` (branch `main`, clean working tree), checked on 2026-10-11.
It is not the intended roadmap. The app is still in early development; a feature appearing in a
menu or module is not evidence of a complete end-to-end workflow. Feature status, with the tests
that back each line, is in [FEATURES.md](FEATURES.md).

## Formats and media

* Archive support is ZIP, 7z, RAR (read only), tar (plain, gzip, bzip2, xz, Zstandard, LZ4,
  Brotli) and single compressed files (`forge-core/src/archive/`).
* Tar: links (symbolic and hard) are never created, not even a hard link to a file in the same
  archive: its entry is skipped and reported. Devices and FIFOs are skipped. Owners and
  permission bits are ignored (a file with no write bit is not made read-only). A name given
  twice extracts its last copy; an earlier copy can't be picked. Global PAX headers are checked
  but not applied. Multi-volume tars are refused, and a tar ends at its first zero block
  (`tar --ignore-zeros` archives, several tars glued together, read as the first one).
* Tar limits: a PAX header of at most 1 MiB, GNU long names of at most 64 KiB, 32 extension
  headers before one file, 100,000 regions in a sparse file's map, 100,000 entries, 256 MB for
  the names and bookkeeping of one listing, the usual size, ratio and decoder-memory limits.
  The memory a compressed tar or file needs (xz dictionaries, Zstandard windows, LZ4 blocks,
  the Brotli window) is read from all its headers and checked against the limit before
  anything is decoded; Zstandard windows over 128 MB and large-window Brotli are refused.
  Reading those headers walks every block header (Zstandard and LZ4 frames don't state their
  length), so a file of more than 4 million blocks in all is refused; that is hundreds of GB
  for real files, and only crafted ones of many tiny blocks get there. That walk reads at most
  64 MB (xz stream padding is read in 64 KB chunks), can be cancelled, and moves the
  "Reading the archive" progress; an xz file with more padding than that is refused.
  Cached listings are kept within 512 MB in all. Names that are not UTF-8 are read as code page 437
  (what Windows' tar.exe writes in ustar headers), not in the code page of the computer that
  made them.
* Listing a compressed tar decodes it to its end (it has no table of contents), and the listing
  is cached while the file's size and time stay the same. Opening, testing or extracting a file
  of a compressed tar also decodes the whole stream: its only checksum is at the end, and files
  are published only once it matched (as in a solid 7z block: up to 4,096 files are held aside;
  with more, the stream is decoded once to check it, then again to extract). Empty files are
  written without waiting. Thumbnails need the whole stream decoded, so they're made for
  compressed tars of up to 64 MB only. A truncated or damaged compressed tar can't be listed at
  all, so nothing can be salvaged from it here.
* A sparse file counts its holes in its size: one whose size is over 1,000 times the data it
  stores is refused as a compression bomb, like any other entry.
* A plain `.tar` has no checksum: *Test archive* reports its files as "can't be verified".
  Brotli streams have none either; Zstandard and LZ4 streams written without one, likewise.
* A single compressed file's size comes from the xz index or the Zstandard frame headers when
  they state it; gzip, bzip2, LZ4 and Brotli files are decoded once to learn it (gzip's stored
  size is modulo 4 GiB and never used). The gzip name is used only when it is a plain, valid
  file name (a path keeps its last part). Single compressed files can't be edited, and only a
  gzip file stores the name of what it holds (when it is ASCII).
* Editing a tar rewrites all of it; the new archive must fit next to the old one (its size plus
  the files added plus 64 MB), and a compressed one is compressed again with its codec's
  Balanced preset, whatever preset made it. `.tar.lz4` is written with the fast LZ4 compressor
  only (no LZ4 HC: the pure-Rust `lz4_flex` has none).
* Windows' own `tar.exe` (bsdtar) can't read or write names outside the ANSI code page (it fails
  on Cyrillic or Chinese names whoever made the archive); ZXraw reads and writes them as UTF-8
  PAX names, which GNU tar, 7-Zip and Python read.
* 7z: split archives (`.7z.001`), self-extracting `.exe` archives (detected, but not opened yet:
  the 7z reader needs a start offset first), archives whose header was
  never finished, and Zstandard, Brotli, LZ4 or Deflate64 blocks (7-Zip forks) are not read;
  the last are reported per file as an unsupported method. New 7z archives use LZMA2 with the
  x86 BCJ filter for executables; BCJ2 and the ARM filters are read but never written.
* 7z AES has no password check of its own. A password counts as proven only when a file
  decodes in full and matches its CRC (ZXraw decodes at most 4 MB of each encrypted block from
  its start looking for one), or when encrypted names pass their checksum. A password that
  fails to decode is refused before anything is written. One that can't be proven (big stored
  files, an archive without CRCs) still lets you extract: every file with a checksum is checked
  and one that fails, "damaged data or wrong password", is never written. Encrypted files that
  store no checksum at all are skipped then ("the password can't be confirmed for this file"),
  and you're asked whether to extract them anyway. Files are not added to such an archive.
* The 7z library (`sevenz-rust2`) keeps its own UTF-16 copy of the password during an
  operation (freed, not wiped, afterwards) and keeps the last AES key it derived in memory
  until it derives another. ZXraw's own copies are wiped.
* 7z limits: headers of at most 64 MB on disk and 128 MB decoded, at most 100,000 entries,
  1 GB of decoder memory per block (256 MB for the header). Anything over them is refused
  before memory is allocated. 7z entries are decoded on one thread.
* In a solid 7z block whose only checksum is the block's (files without CRCs of their own),
  extracted files are held as temp files and published only once the whole block has been
  decoded and its checksum matched, so extracting even one such file decodes its block to
  the end. *Test archive* reports encrypted files without any checksum under an unproven
  password as "can't be verified", never as OK.
* Self-extracting zips (a program stub in front of the archive) can be opened and extracted
  but not edited: an edit would drop the stub. A self-extracting `.exe` never opens on
  double-click (it still runs); right-click › *Open as archive* opens it as a folder. That
  choice lasts until ZXraw closes (up to 32 files). Detection reads the program's PE header and
  searches at most 16 MB after its image for the archive, keeping only a structure that checks
  out (a 7z signature inside the program loses to a valid zip after it). A self-extracting zip64
  is found only by its end record, as before. A self-extracting 7z is recognised, but opening it
  reports "can't be opened yet", and editing it is refused like a zip's.
* Editing a 7z recompresses the solid blocks that lose a file. Those blocks are held to the
  extraction limits (decoder memory, total size), and their kept files are spilled to a temp
  file next to the archive, so that much free space is needed there; otherwise the edit is
  refused before anything is written.
* Pause and cancel in 7z: while compressing they are checked on every read of a source file;
  while decoding a solid block up to the wanted files, every 256 KB. One case is coarser: when
  a file being extracted fails (a disk error), the rest of its data is decoded without checks.
* A 7z "update" archive (one with deletion markers, anti-items) can be opened and extracted
  (the markers are ignored) but not edited. A 7z entry name containing NUL makes the archive
  unreadable rather than listing that entry as unsafe.
* RAR is read only. ZXraw never creates or edits a RAR archive: the UnRAR licence forbids
  re-creating the RAR compression algorithm. In-archive delete, add and cut are refused with a
  message (a move out copies and leaves the items in the archive); copy and drag out work.
* RAR hard links and RAR5 "file copy" references (made by `rar -oi`) are listed but not extracted.
  Symbolic links and junctions are not extracted, as in the other formats.
* A self-extracting RAR (`.exe`) doesn't open. Recovery records and recovery volumes (`.rev`) are
  not used to repair a damaged archive.
* A multi-volume set opens from any of its volumes only when its first volume is present; a
  missing later volume is named.
* RAR4 has no password check, so a wrong password shows as "damaged data or wrong password", as
  damage does. RAR5 tells the two apart.
* A RAR whose dictionary is over the decoder-memory limit (1 GiB by default) is refused before
  extracting. UnRAR itself allows 4 GB.
* UnRAR itself turns `:` in RAR names into `_` (such files extract under that name) and blanks a
  name with `..` in it; a blanked name counts as unsafe, so the archive refuses extraction, as
  other formats do with unsafe names.
* **Extraction destination.** The destination is resolved once when the job starts (before the
  archive is planned), and every folder from the volume root down stays open until the job
  ends, so none can be renamed, deleted or replaced by a junction meanwhile. A folder the
  extraction makes (*Extract to `<name>\`*, smart, selections) is held from the moment it is
  made, and taking back a cancelled extraction happens while it is still held. Each folder is
  opened relative to the one above it, and the last must be the folder the path resolved to, or
  the job is refused. Taking back removes only files the job created at empty paths (a file it
  wrote over an existing one keeps the new copy) and folders it made, and only when the Windows
  file ID proves it is still the same item: anything replaced meanwhile is left in place and
  reported. On volumes without a file ID (some FAT and network volumes) nothing is removed;
  it is left and reported. Not covered:
  changes before the job starts (a junction or mapped drive in the chosen path is followed, as
  it is then); in the instant between making a new folder and holding it, someone allowed to
  rename in its parent can swap it for another real folder (never a link), and files then still
  land at the chosen path; on a network share the hold depends on the server enforcing Windows
  sharing rules; a volume mounted in an NTFS folder (no drive letter) is refused as a link; the
  *Extract them anyway* rerun makes its folder by path.
* Temp files left by a crash or an older version (exactly `.forge-partial-<32 hex>.tmp`, or the
  old `.forge-partial-<16 hex>`) are removed from an extraction's destination and the existing
  folders it writes into when a job starts, once over an hour old. A file in use, a folder, a
  link or a hard-linked file with such a name is always kept. Leftovers in folders ZXraw never
  extracts into again stay until it does. At startup `%TEMP%\ZXraw Archive Temp` loses folders
  (and stray temp files) over a day old, except the drag-out folders of a ZXraw still running;
  links there are removed as links, never followed.
* Archives are told apart by a stamp of their size, modified time, file identity and their
  first 4 KB and last 64 KB. Where the file identity is read (local NTFS, most shares), any
  replacement is seen. Where it isn't (some network shares, FAT-like volumes), the stamp can't
  rule out a same-size, same-time replacement that differs only in the middle, so the archive is
  listed again on each folder open and temp copies are made again. Replacements are seen there
  too, at a cost: a compressed tar (decoded to list) opens slowly.
* Inside an open zip, 7z or tar, rename and new folder are not available, and copying between two
  archives is refused. Photos inside a zip open with their default app, not the built-in viewer.
* Items dragged or copied out of an archive to other apps are virtual files
  (`FileGroupDescriptorW` + `FileContents`), extracted into a private folder under
  `%TEMP%\ZXraw Archive Temp` as the target reads them. That folder needs free space for the
  selection and is removed when the target releases the items (after a paste, when something
  else is copied; leftovers after a day). Apps that only take real file paths (`CF_HDROP`)
  can't take them. The files they write don't carry the archive's "downloaded from the
  internet" mark (an extraction in ZXraw does), since the virtual-file protocol has no way to
  pass it. A file whose only checksum is its solid 7z block's is handed over only once the
  whole block was decoded and matched. Encrypted files without any checksum, under a
  password nothing could prove, are refused there (extract them in ZXraw to choose "Extract
  them anyway"). If the archive file changed since its pane listed it, every file is refused
  (the pane's listing no longer describes it): open it again and copy again.
* Archive items on the clipboard can't be pasted after ZXraw closes: their data comes from the
  running app, so the clipboard is emptied on exit rather than flushed.
* A move out of an archive (Ctrl+X, Shift-drag, F6) removes the items only when the target
  reported a successful move *and* read every file to its end, the archive is the same file,
  unchanged, as when the items began to be read, and you confirm. "The same file, unchanged"
  is its volume and 128-bit file id, size, write time and a hash of its directory region
  (a zip's zip64 and end records and central directory, a 7z's headers, else its first and
  last 64 KiB), recorded from a handle that shares only reading and is kept while the items
  are read, so nothing can write, rename or delete the archive meanwhile. The removal job
  opens the archive the same way, checks that identity on the handle, copies the archive
  through it to a private temp file next to it, and edits only that copy. It then opens the
  original again (sharing reading and deleting), checks it again, and renames the edited copy
  over it through the copy's own handle. Entries are only ever removed from a verified copy:
  if another program moves the original away and puts a file in its place between that last
  check and the rename, that file is replaced by the edited copy (nothing is removed from
  it, and the moved original is unchanged; the job says so). A program already writing to
  the archive is waited for, up to 10 s, then the removal is refused. Once the target copies
  asynchronously, only the end of its operation counts:
  a failed end cancels the move whatever it said before. Anything less (a copy, a skipped
  file, an error) leaves the archive as it was. A target that never reports the result (some
  apps don't) leaves it as a copy. Exactly the entries that were handed out are removed, never
  a folder with whatever it holds: links and names that clash on Windows are not handed out,
  so they stay, and so does the folder holding them (the confirmation says how many). A name
  another entry also answers to is never deleted. An archive holding names that differ only
  in case (`A.txt` and `a.txt`, or a file `docs` next to a folder `docs/`) is never edited by
  a move at all: rewriting a zip keeps only the first of such entries. Delete and adding files
  rewrite it the same way, so they also drop the later of two such entries today.
* ZipCrypto (legacy ZIP encryption) checks a password with one byte, so a wrong password is
  caught up front 255 times in 256; otherwise the entry fails its checksum and is reported as
  "damaged data or wrong password". AES archives always tell the two apart.
* New AES-encrypted entries follow WinZip's recommendation, as the `zip` crate writes them:
  AE-1 (with a CRC check) for files of 20 bytes or more, AE-2 (no CRC, so tiny contents can't be
  guessed from it) below that. Both are AES-256.
* Archives whose names collide on Windows (names that differ only in capitals, such as `A.txt`
  and `a.txt`; a file and a folder of one name; one name given twice, which tar allows) can't
  be edited: deleting or adding files is refused before anything is written, as an edit keeps
  and drops entries by name and could lose one it wasn't asked to delete. Extract them instead.
* Editing a zip copies the entries it keeps exactly as stored, so it needs no password unless
  files are added to an encrypted zip. New files then use the password you enter; if the zip's
  entries use several passwords, you're told, and the new files get the one you entered.
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
* Sizes come from folder listings, as in File Explorer. NTFS refreshes the size stored in a
  listing only when a file is closed, and only for the name it was written through. A file
  still being written is read again when its listing says 0 bytes (local folders only), but
  one that already had a size, or the other names of a hard-linked file, can show an older
  size until the file is closed and the folder is listed again. Folder totals add up those
  same sizes, count a hard-linked file once per name, and never follow symlinks or junctions.
* Git status badges are on by default (turn them off in Settings › Browsing). The scan treats
  every repository as hostile: it never runs a program, opens nothing by path below a local
  drive's root, never follows a link, never opens a cloud placeholder, and caps every read,
  count and the time taken. Submodules, linked worktrees and clones made from a local path
  (hard-linked objects) show no badges, and SUBST drives and network shares are skipped; the
  full list of limits follows.
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
  do they on a share, a mapped network drive or a SUBST drive (an alias for a folder path,
  which could hold links), below a junction or symlink anywhere in the folder's own path (it
  is reached from the drive's root without following links), inside
  OneDrive or another cloud-synced folder, nor submodules and linked worktrees (a
  `.git` file could name any place, so `.git` must be a real folder), nor repositories with a
  link or a hard-linked file anywhere among the Git files read (so clones made from a local
  path, whose objects Git hard-links, show none), a work tree or `core.excludesFile` outside the
  folder, any `objects/info/alternates` entry (so `git clone --shared` clones show none), a
  split index, SHA-256 objects, or an index with a path over 4 KB or 256 folders deep, or a
  cached tree deeper than 256 levels. A shallow or partial clone missing `HEAD`'s objects shows
  no staged changes. A repository nested inside another, untracked one is shown as a plain
  new folder. Nothing is opened before its folder's listing says it is not a cloud
  placeholder: an online-only Git file makes the repository show no badges, an online-only
  `.gitignore` is skipped and an online-only folder is not entered (nothing is downloaded).
* The Git scan opens nothing by path but the root of a local drive; everything else is reached
  one name at a time without following links. A hard-linked tracked file whose time changed
  counts as modified (it is never read). The symlink-to-share tests need developer mode and
  were skipped on the development PC; junctions are tested.
* Files and folders can be dropped into the app from other applications. The drop goes to the
  folder under the pointer and moves within one volume (copies across volumes, Ctrl copies,
  Shift moves), like a drag inside the app. winit's drop target always answers the source with
  "copy", so the source never deletes the originals (ZXraw does the move itself), but the drag
  cursor shows the copy badge even when the drop will move; changing that would need ZXraw's own
  `IDropTarget`. Volumes are compared by drive letter or share first, so a drop between two
  names of one volume (a disk mounted in a folder and its own drive letter, or a `subst`
  drive) copies instead of moving, which is the safe side. A link or junction that lives on,
  points to, or is dropped onto a network location is refused with a message, so a drop
  never follows a link onto a share; network paths are recognised without a network call,
  but `GetDriveTypeW` on a disconnected mapped drive is not guaranteed to answer at once.
  During the drag the pointer position is read from Windows each frame; e2e
  tests simulate the drop through the automation hook, not a real OLE drag. Dragging files past the window edge starts a
  Windows shell drag (`forge-win/src/dragout.rs`) that offers copy and link only, never move.
  The data object and the trigger are unit tested, but a drop into Explorer or another
  application has not been verified: it is modal and cannot be automated here. The drag starts
  only at the window edge, so the drop target must be outside ZXraw. At most 500 items can be
  dragged out at once (each costs a shell parse on the UI thread); larger selections are
  refused with a message pointing to copy and paste.
* The Windows file clipboard (`CF_HDROP`, preferred drop effect, text paths) passes its engine
  tests in `forge-win/src/clipboard.rs`. Explorer's paste of ZXraw's copy or cut is tested
  through the shell's paste verb. Paste into other applications has not been checked.
* A cut pasted elsewhere is noticed only by its items leaving their folders. One thread checks
  the newest cut every 0.75 s; a check that takes longer than 5 s (a share that hangs) counts
  as "could not tell", and its items stay cut. An item counts as gone when it is missing twice
  in a row while the folder above it can be read, or when a different item has taken its name.
  An item that comes back later (for example, undo in Explorer) is not cut again.
* A cut item is identified by its Windows file ID, or, where the file system has none, by its
  kind, size, creation and modified time together. An item whose identity cannot be read (or
  could not be checked again at paste time) is left out of a paste with a message, never moved
  unchecked. Without a file ID, editing a cut file counts as replacing it, so it leaves the cut.
  The move itself checks each item against its identity from the cut once more when it
  starts, so an item replaced while the move waited in the queue is not moved either.
* All clipboard writes go through one worker, newest request first. ZXraw recognises its own
  clipboard content by a private "ZXraw Explorer Clipboard Owner" format and only ever replaces
  or empties that; another app's identical list is left alone.
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
  (`forge-win` `shell::tests`, run with `--ignored`) and manual use. The sandbox refuses items
  whose name starts with `not-recyclable`, standing in for the Recycle Bin's "too big" refusal.
* The administrator prompt for deleting from protected folders is Windows' own dialog (then
  UAC), not the app's styled one. It is owned by the main window so it opens in front.
* Recycling a big folder takes as long as Windows needs, and its bar cannot measure most of
  it. Windows first counts every file in the folder (the popup shows how many it has found),
  then moves the folder in one step and does its own bookkeeping for each file, reporting
  nothing until it ends (the bar slides). Measured with `forge-win`'s `recycle_bench` example
  on 200,000 small files on an SSD: about 4 s counting and 11 s in that last step, 15-16 s in
  all, the same with or without ZXraw's progress sink, with `SHFileOperation`, and with the
  shell's own progress window as Explorer runs it. Pause and Cancel cannot interrupt that
  last step: pressed during it, the popup says "Pausing (or Stopping) after the current
  folder…" and they take effect when it ends.
* An Undo cannot be paused or cancelled: its progress popup has no buttons, and it runs to
  the end. One Undo of any kind (operations, renames, text replacements) runs at a time;
  pressing Undo while one runs only says so.
* The programs named for a file in use come from Restart Manager, which looks at the first
  5,000 files of a folder and can miss services and kernel holders.
* Where a file system has no stable file IDs (some network shares, index 0), "the same item"
  rests on type and creation time (plus size and write time for files). A folder replaced by
  another created at the very same instant would pass; no missing time ever counts as a match.
* Accepted risk: on drives without stable file IDs (FAT and exFAT USB sticks and cards, some
  network shares), a folder deleted and recreated under the same name while the "Delete
  permanently?" question is open can look identical to the one the user was asked about.
  Windows' file-system tunnelling gives the new folder the old one's creation time, and a
  folder's identity there is only its type and creation time. Confirming the question would
  then delete the new folder.

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

## Archive file types and the Explorer menu

* On Windows 11 the ZXraw menu is under *Show more options* (the classic menu). A top-level entry in
  the new menu needs an `IExplorerCommand` with package identity (a sparse MSIX package, signed), which
  is not done.
* Static menu verbs cannot show the selected item's name, so the menu says `Add to "<name>.zip"`
  literally and *Extract to a folder named after the archive* instead of the actual names.
* Archive defaults must be confirmed by the user in Windows Settings; ZXraw cannot set them itself
  (Windows protects the `UserChoice` of archive types and ZXraw does not forge it). Registering the
  types does not change their class default either, so a double-click keeps opening the previous app
  (for `.zip`, Windows' compressed folders) until then.
* A menu command runs on the whole selection or not at all: an item without a file system path (a
  library entry, a phone, *This PC*), an item that vanished, or a non-archive given to an extract
  command refuses the command with a message naming the items. So does a selection Explorer
  could not list completely; the menu never falls back to a shorter list of the selection.
* `.rar` is registered for the menu and file types, but the extract commands open it only once
  ZXraw reads RAR (extract only: no creating `.rar`). Until then they report it as not an archive.
* A selection of more than 8192 items, or over 4 MiB of paths, cannot be handed to the running window
  and runs in a new window instead.
* Elevated (administrator) Explorer windows ignore per-user COM registrations, so the menu commands do
  nothing there.

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
* A launch with a path is handed to the running window (see [FEATURES.md](FEATURES.md)). Elevated
  (administrator) and normal windows are separate groups, so a launch from the other level opens its
  own window. If the running window is hung, the launch waits about 1.5 s and then opens its own
  window (logged); while the running window is still starting, it waits up to about 3 s for its
  first frame; a closing window sends it on at once. Bringing the window to the front relies on
  Windows' foreground rules (`AllowSetForegroundWindow`), which no automated test covers.
* A launch is never lost, but it is not guaranteed to open exactly once: when in doubt the new
  launch opens its own window. Under rare scheduling delays (the running window took the launch but
  its answer arrived after the new process stopped waiting) the launch opens both as a tab and in a
  window of its own. This extra window is accepted for folder and file launches, which have no commit
  step. Explorer menu commands do have one (protocol v3), so they run once, except that a sender stalled
  for 60 s between reading the answer and confirming can lose one. When the sender cannot tell
  whether its confirmation arrived, it shows an error ("check the ZXraw Explorer window") instead of
  running the command itself: a missed command is preferred over a command run twice.

* The protocol version is part of the pipe name. While an update overlaps (the old version's window
  still open, the new version launched), launches of the new version open their own window.
* A handed-over path that no longer exists when the running window gets to it is treated like a cold
  start with a missing path: a missing local folder or file opens nothing (no tab, no message), and a
  network path, which is never probed first, opens a tab whose listing reports the error.
* The hand-off pipe only trusts a process of the same user, session and integrity level that runs
  the same executable file. Another program of the same user is otherwise outside the threat model:
  it can already do far more to that user than receive a folder path. Creating the pipe first
  could stop launches from reaching ZXraw's window (each would open its own window), never more.

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
  requests an hour per IP address, so on a shared network the API can refuse; the check then
  reads the public `github.com/.../releases/latest` redirect instead, which gives the version
  but no release notes. Only if that fails too does it stay silent and try again the next day
  (**Check now** shows the error). The installer is
  checked against `SHA256SUMS.txt` from the same GitHub release, which protects against a
  corrupted or swapped download but not against someone who controls the release itself; the
  installer is not code-signed.
* On Windows, an in-app update closes the app and runs the installer silently. If another ZXraw Explorer
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
* Renames (single, batch, case-only and Undo) first move each file to a temporary name in its
  own folder, `.forge-rename-<process>-<run>-<item>-<n>.tmp`, then to its final name, so a
  rename never replaces an existing file. If the app or Windows stops between the two steps,
  the file stays under that temporary name; nothing is deleted. Nothing scans for these
  files at startup. To recover one, rename it back by hand. The library rows and Undo still
  point to the name before the rename, so the original name is the safe choice. Files with
  this prefix are never created any other way.
* Rename, New folder and New file run one at a time per location: one queue for local
  drives and one per network share. A share that stops answering holds up only its own
  queue. While any of them is still running, Undo waits for it, because Undo always undoes
  the most recent action.

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

## Linux

The Linux build (X11 and Wayland) is new. Browsing, the viewer, culling, metadata, archives,
the index, search, Git badges, copy, move and rename work. Everything Windows-only is hidden
or refuses with a message; nothing should crash for lack of it.

* Hidden on Linux (menus, command palette and shortcuts): Explorer menu
  and file type registration, app icon shortcuts, *Open terminal as administrator*, and the
  FFmpeg download. Install `ffmpeg` with the package manager: there is no versioned,
  checksummed static Linux build to pin the way the Windows one is pinned.
* Default app (Settings › Default app) uses the freedesktop MIME Apps mechanism. It writes only
  user-level entries (`~/.config/mimeapps.list`, and a desktop file such as
  `x-cinnamon-mimeapps.list` if that overrides a type), never `/usr` or `/etc`. Desktops that
  ignore `mimeapps.list` are not covered. The folder switch only affects apps that ask the MIME
  system to open a folder; a desktop's own *Files* launcher or shortcut may still open its file
  manager. Turning a switch off restores the recorded apps, but not uninstalled ones. If the
  record of the previous apps (`linux-default-apps.tsv` in the config folder) is damaged,
  nothing is restored: set the defaults in the desktop's own settings. A `mimeapps.list` that
  another program keeps changing while ZXraw saves it is left alone with "try again". An
  AppImage moved while it runs has to be started again from its new place before the menu
  entry can be updated.
* Through the Linux backends in `forge-win` (see
  [Linux platform behaviour](#linux-platform-behaviour)): the Trash (freedesktop), the file
  clipboard, single instance (a second launch opens a tab in the running window), *Open with*
  (XDG portal, else the default app), *Properties* (`FileManager1.ShowItemProperties`), *Show in
  file manager*, and network locations (`gio mount`; shares that need a login fail with an
  error that says so: mount them once in the file manager).
* Updates: the daily check works. "Update now" installs the new version on Linux in two cases:
  for an AppImage, which replaces itself with no password, and for the `zxraw` .deb, which asks
  for your password through the desktop's polkit agent. Any other copy (a build folder, a copied
  binary, an AppImage moved while it runs, a non-x86_64 PC) gets the release page, as before.
  An AppImage counts only when the app runs from its mount (`$APPDIR`) and `$APPIMAGE` is a
  regular file, not a symlink, owned by you, with the type 2 AppImage header; a symlinked
  AppImage gets the release page.
  Nothing installs by itself. After an install, "Restart now" closes the window through the normal
  shutdown (settings and session are saved) and starts the new copy.
* An AppImage is replaced by renaming the verified new file over the old path, which is atomic.
  No `.old` backup is kept. The file name keeps the old version number, but the path stays the
  same, so a menu entry made in Settings › Default app keeps working. If the folder is not
  writable, or the AppImage belongs to another user or group, the verified file is saved in
  Downloads (or your home folder) and the notice offers to open that folder.
* The .deb is installed by a helper the package ships, `/usr/lib/zxraw/install-update`, run
  through `pkexec` (the prompt says "Update ZXraw Explorer" where the desktop shows polkit
  action messages). Before asking for the password, the app checks the download again (SHA-256)
  and its package fields with `dpkg-deb` (`zxraw`, the offered version, amd64). As root, the
  helper copies the file once into a new root-only folder, checks the copy's SHA-256 and fields
  again, and installs only the copy with `apt-get install --no-remove`, so the file cannot be
  swapped after the check. A .deb installed before the helper existed has no helper: its first
  update opens the downloaded package in the Package Installer instead (finish it there, then
  restart). Updates after that use the helper. Dismissing the password dialog keeps the .deb for
  opening by hand.
* Drag and drop works on X11 (see [Linux platform behaviour](#linux-platform-behaviour)).
  winit has no drag and drop on Wayland: turn on Settings › File operations › *Use XWayland
  (enables drag and drop)*, or start with `ZXRAW_X11=1`, and restart. The app then runs as an
  X11 client of XWayland (`ZXRAW_X11=0` turns it off again). Items inside an archive can't be
  dragged out on Linux; copy and paste them.
* The sidebar lists `/` and the mounts under `/media`, `/run/media` and `/mnt`.
* File dialogs need a running `xdg-desktop-portal` with a FileChooser backend; without one they
  don't open.
* Names are case-sensitive: `IMG.jpg` and `img.jpg` are two files everywhere (index, notes,
  tags, collections, Git badges, pins). Hidden files are names that start with `.`.
* Git badges: a repository is read with `openat` and `O_NOFOLLOW` one name at a time from `/`.
  Symlinks are never followed, and mount points inside the repository are skipped like links
  (another device, another mount id or `STATX_ATTR_MOUNT_ROOT`, so bind mounts too). On a
  kernel that reports neither (before Linux 5.8) no folder is entered and no badges show. FIFOs and
  devices are never opened. Folders on network or FUSE file systems (NFS, SMB, sshfs, rclone
  and others) show no badges. File names that are not UTF-8 are skipped, so such a file gets
  no badge. Without `/proc`, the check that every opened folder is still inside the
  repository is skipped; the walk still opens only single names relative to folders it holds.
* Extraction creates every folder with `mkdirat`/`openat(O_NOFOLLOW)` relative to the folder
  above it, and writes, renames and deletes its temp files through `/proc/self/fd` paths of
  those held folders. Without a usable `/proc` it refuses to extract.
* A library database copied from Windows is rebuilt case-sensitive the first time it is
  opened on Linux.
* File identity (archive stamps, safe cleanup after a cancelled extraction, replace checks)
  uses device, inode and birth time. On a file system without birth times it falls back to the
  weak identity, so cleanup reports what it left instead of removing it.
* Unix has no share modes and no rename or delete through a handle. A file the app holds open
  can still be changed, renamed or deleted by other programs. Publishing a finished file checks
  afterwards that the object renamed is the one held open (device and inode) and renames it
  back otherwise. Safe removal moves the item into a private staging folder, checks it there,
  then deletes it; another program of the same user could still interfere inside that folder,
  which Windows' delete-through-handle rules out.
* Volume identity is per path root, and every Linux path is under `/`. Library roots are not
  told apart by mount, so "offline volume" detection does not work for unplugged drives yet.
* The `e2e` UI test suite runs on Linux too, each app run on its own `Xvfb`. Scenarios for
  Windows-only features (OLE drop target, the Windows installer update, FFmpeg download, network
  shares, the hidden attribute) are compiled on Windows only; see [TESTING.md](TESTING.md#linux).

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

## Linux platform behaviour

The Linux port of the platform layer (`forge-win`, plus HEIF in `forge-media`) differs from
Windows in these ways:

- **Trash:** items on a mount with no usable trash directory (no writable `$topdir`, or a
  `.Trash` that is not sticky and no way to create `.Trash-$uid`) are reported as not
  recyclable, and the app asks before deleting permanently. If the home trash is on the item's
  filesystem but can't take it, there is no fallback to a mount trash. A folder that contains
  the trash can't be recycled. Recycle progress is per item (each item is one rename).
- **Permanent delete (staging and recovery):** each item is first moved into a new private
  folder next to it, named `<name>.zxraw-deleting-<pid>-<n>` (visible, not a dot file), and
  deleted there, so a file put at the old path meanwhile is never touched. If the delete is
  cancelled or fails part way, what is left is moved back under its name. If that name was
  taken in the meantime (or the move back fails), the item is left in the staging folder and
  the failure message names the exact path. After a crash or power loss mid-delete, any
  `*.zxraw-deleting-*` folder holds what was not yet deleted: move its contents back (or
  delete the folder) by hand. Accepted risk: on file systems without `RENAME_NOREPLACE`
  (vfat, exFAT and some network file systems), the move back checks that the name is free and
  then renames, so something created at that name in the few system calls between the check
  and the rename would be replaced. A volume mounted inside a folder being deleted (found in
  the mount table, by a different device, or by `statx`'s mount-root flag) makes the delete of
  that folder fail; nothing under the mount point is deleted. When the mount is only found
  partway through (it appeared after the delete started), entries deleted before it was
  reached stay deleted and the rest is moved back. If neither the mount table can be read nor
  the kernel reports mount roots (`STATX_ATTR_MOUNT_ROOT`, Linux 5.8+), folders are not
  deleted permanently at all (files still are). If the empty staging folder can't be removed
  after a successful delete, it is listed as a separate failure under its own path.
- **Clipboard:** X11 and Wayland clipboards belong to the running app, so copied files vanish
  from the clipboard when ZXraw Explorer exits unless a clipboard manager keeps a copy.
  There is no system clipboard sequence number: the sequence is a hash of the owner window,
  its ownership time and an in-process counter (Wayland: the offered formats and owner token).
  Two foreign copies from the same window can look the same if the owner doesn't answer within
  250 ms. Compare-and-swap calls (`set_files_if_unchanged`, `replace_files_if_token`) are atomic
  only within our process. Relative paths are rejected. The Wayland backend needs the
  data-control protocol (wlroots compositors, KDE; not GNOME Mutter, where it falls back to
  XWayland) and has not been run against a real compositor yet.
- **Open with:** there is no portable "Open with" dialog. The XDG desktop portal's chooser is
  used when available, otherwise the file opens with its default app.
- **Properties / reveal:** need a file manager that implements `org.freedesktop.FileManager1`
  (Nemo, Nautilus, Dolphin, Caja, Thunar do). Reveal falls back to opening the parent folder;
  properties has no fallback and reports the error.
- **Network shares:** `gio mount` can't prompt for a password, so shares that need a login
  fail with an error that says so; mount them once in the file manager first. Drive-letter
  mappings don't exist (`mapped_share` is `None`). Only CIFS/SMB mounts count as network roots
  (NFS and sshfs are treated as local by the drag-and-drop network check).
- **Drives:** each device is listed once (btrfs subvolumes show as `/` and `/home` only). The
  drive serial comes from mountinfo; on btrfs `st_dev` differs per subvolume, so a file's
  volume serial can differ from its drive's serial. `physical_disks` is not implemented.
- **File attributes:** Linux has no hidden attribute (hidden means a dot-file), so `hidden` is
  ignored; read-only maps to the write permission bits. `rename_open_file_over` resolves the
  open file through `/proc/self/fd` and renames by path, so it is not atomic against a rename
  of the same file by someone else in between.
- **File holders:** "in use" means "open by some process" (Linux has no mandatory locks);
  processes of other users are skipped.
- **Single instance:** forwarded paths that are not valid UTF-8 are passed lossily, since the
  payload format is shared with Windows. Session and integrity-level checks have no equivalent.
- **Downloads:** TLS trusts the bundled Mozilla roots, not the system store, so a corporate
  TLS-intercepting proxy's certificate is not trusted.
- **Drag and drop in (X11, XWayland):** `forge-win`'s XDND target replaces winit's. It sets
  `XdndProxy` on the window to point to a window on its own X connection, so every XDND message
  reaches it and winit never sees the drag. While files hover, the pointer and Ctrl/Shift are read
  with `QueryPointer` each frame, so the folder under the pointer lights up and takes the drop,
  with the same copy/move rule and conflict dialogs as on Windows. The source is told the real
  copy or move in `XdndStatus`. A move is done by the app and reported in `XdndFinished` as
  `XdndActionPrivate`, so the source never deletes anything. Only `text/uri-list` is read;
  non-UTF-8 names and symlinks come through as they are. A source that offers no file URIs (a
  browser image, text) is refused.
- **Drag and drop out (X11, XWayland):** dragging items past the window edge starts an XDND
  drag from a background thread, so the window keeps drawing. Copy is the default, Shift moves
  and Ctrl+Shift links. winit holds the pointer grab while the button is down, so the source does
  not grab: it polls the pointer, the button and Esc (`QueryPointer`, `QueryKeymap`). The cursor is a
  cursor-font glyph set on our window, which X shows during the grab: a hand when the target
  accepts, a circle when it refuses. It does not tell copy from move, and the desktop's cursor
  theme isn't used. Offered: `text/uri-list`, `x-special/gnome-copied-files` and plain text.
  A `DELETE` request is refused, so a move is always done by the target (a file manager
  moves the files itself). Afterwards the panes showing the items' folders are re-listed.
  Tested against Nemo. Dropping back onto ZXraw itself does nothing; use an in-app drag.
- **Virtual files, private desktop:** not implemented on Linux and return `Unsupported`.
- **Window placement:** `maximize_hidden` is unsupported; the app maximizes through egui.
- **HEIF/HEIC/AVIF:** only with the `heif` cargo feature and libheif installed. Embedded HEIF
  thumbnails are not used (same as WIC). EXIF orientation on HEIF is applied but untested
  for lack of a sample; HEIC decoding was tested on sample files only (no local HEVC encoder).
- **Shell integration** (context menu, file types, default apps, Explorer verbs, Everything
  search, shell notifications) is Windows-only. Default apps on Linux are under
  [Linux](#linux).
