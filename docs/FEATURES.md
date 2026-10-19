# Feature checklist

Checked against commit `dfd43d9` (branch `main`, clean working tree) on 2026-10-11.

Verification run for this pass (see [TESTING.md](TESTING.md) for commands):

* `CARGO_TARGET_DIR=target/docs cargo test --workspace --no-fail-fast`: 396 passed, 0 failed,
  2 ignored. The ignored tests are opt-in: `demo_contact_sheet_from_photos` (writes a demo
  sheet from a hard-coded folder) and `thumbnail_throughput_24mp` (benchmark, release only).
* `cargo test -p forge-app --test e2e -- --test-threads=1`: 10 passed, 0 failed.
* FFmpeg and ffprobe are not installed on this machine, so no video tool was run live.

Status means:

* ✅ a test, benchmark, or live check in this checkout passed for the stated path.
* 🟡 source exists, but the complete workflow or user-visible behaviour is partial,
  unverified, or still changing (including "landed, verification in progress").
* ⬜ no implementation was found in the current source.

A passing engine test does not prove visual correctness or Windows shell interoperability.
The exact commands and limits are in [TESTING.md](TESTING.md) and
[BENCHMARKS.md](BENCHMARKS.md). Known gaps are listed in [LIMITATIONS.md](LIMITATIONS.md).

## Navigation

* 🟡 Browse a directory by path, open parent folders, and keep back/forward history —
  `forge-app/src/pane.rs`. The e2e scripts navigate to folders by path; back/forward history
  and parent-folder steps are not asserted by any test. Only the 50k-folder engine listing is
  measured.
* 🟡 Tabs, dual panes, session restore, address/breadcrumb navigation, and drive sidebar —
  `forge-app/src/{app,pane,breadcrumb,sidebar}.rs`. Tabs restored after a restart and
  dual-pane copy/move are e2e-verified (`tabs_are_restored_on_the_next_start`,
  `copy_to_other_pane_duplicates_files_byte_for_byte`). Each drive shows a usage bar with its
  free space in small text under it (`142 GB free of 476 GB`, unit-tested in
  `sidebar::free_space_tests`). The address bar, breadcrumb UI, and drive sidebar are not
  automated.
* 🟡 Quick access pins for files and folders — `forge-app/src/sidebar.rs`. Pin with *Pin to Quick
  access* (file menu or palette), *Pin this folder to Quick access* (Ctrl+D) or by dragging onto the
  section. A pinned file opens with its default app, started in its own folder; a pinned folder
  opens in the pane. Pins reorder by drag and have rename, reset name, move up/down, open containing
  folder and unpin. A pin whose target is missing is greyed out with *Locate…* and *Remove*;
  availability is checked in the background (every 5 s locally, every 30 s on network paths), so an
  offline share does not freeze the window. Folder bookmarks move here automatically, and the
  separate Bookmarks section is gone. The rendered sidebar is not automated.
* 🟡 Network locations (NAS and shared folders) — `forge-app/src/sidebar/network.rs`,
  `forge-core/src/netpath.rs`, `forge-win/src/network.rs`. The sidebar's Network section lists
  saved shares and mapped network drives, each with a status dot: online, offline or unreachable,
  needs sign-in, or checking. Status comes from background checks on the shared probe service
  (every 30 s, and when the section title is clicked), so an offline NAS never freezes the
  window, and clicking one says it is offline instead of opening it. *Add network location…*
  (the + button) takes a path such as `\\nas\photos`, `//nas/photos` or `smb://nas/photos`,
  checks it without contacting the server, and saves it with a name; rename and remove are in
  the context menu. *Add to Network…* is also in the file list's context menu and on the
  address bar's segments when the folder is on a share. *Connect…* (or clicking a share that
  needs sign-in) lets Windows show its own sign-in dialog: the app never asks for, sees or
  stores a password. Background work is gentler on network paths: thumbnails of network
  files, images and videos alike, share one pool of 2 workers (local images keep half the
  cores), duplicate scans
  with a network root use 2 workers for partial hashes and 1 for full hashes (local: up to 8
  and 4), and in queue mode a copy to or from a network location never runs side by side with
  another transfer (`forge-core/src/netpolicy.rs`). Verified by unit tests (path validation,
  status classification with an injected probe, settings round trip, worker selection) and
  the e2e `sidebar::network_locations_are_added_renamed_and_removed_without_freezing_on_an_offline_share`
  against an unroutable test address; no real NAS is used in tests.
* 🟡 Saved layouts (workspaces) — `forge-app/src/layouts.rs`. *Save layout…* stores the open tabs:
  each tab's left and right folders, view mode, sort, split state and active tab. Load from the
  sidebar's Layouts section or *Load layout…* in the palette; *Manage layouts…* renames, reorders and
  deletes. Saving under an existing name updates that layout. Missing folders are skipped and listed
  in a message. Settings › Browsing's *Reopen the last session's tabs at startup* (on by default) is
  unchanged, and sessions now also remember each pane's sort. Layouts and pins live in the settings
  file, and older settings files load unchanged. No test result is recorded for this yet.
* ✅ Sidebar section headers — `forge-app/src/sidebar.rs` (`header_row`, `collapsible_header`).
  Every section uses one header row, so the titles and Network's *+* line up. Layouts and
  Collections are collapsible: an empty section starts collapsed, one with items open, and a
  click on the title remembers the choice in the settings (`sidebar_sections`). Their ⋯ button
  opens a menu that explains the feature, then *Save current layout…* / *Manage layouts…* or
  *New collection…* / *Manage collections…*, each with a tooltip. e2e
  `sidebar::empty_sections_start_collapsed_and_their_menus_open_the_layout_and_collection_windows`.
* ✅ Stream directory entries in batches (engine) — `forge-core/src/listing.rs`
  (`lists_unicode_and_streams_batches`, `junctions_are_links`). The 50,000-entry run in
  [BENCHMARKS.md](BENCHMARKS.md) completed with zero listing errors.
* ✅ Cancel a listing (engine) — `forge-core/src/listing.rs::cancel_stops_listing`. Cancelling
  a superseded listing from the rendered UI is not exercised.
* 🟡 Browse a 50,000-file folder in the desktop UI — the screenshot hook loaded the folder and
  captured a frame in an earlier run (not repeated for `dfd43d9`); see [BENCHMARKS.md](BENCHMARKS.md).
  Frame pacing and interactive responsiveness are not measured.
* ✅ Type to filter (Settings › Browsing › *Typing in the file list*: *Jump to name*, the
  default, or *Filter the list*). In filter mode typed letters go into the folder filter, so the
  same query syntax applies; Backspace removes a letter, Esc clears it, and the status bar shows
  the active filter — `pane.rs` (`typing_extends_the_filter_and_backspace_and_escape_undo_it`,
  `typed_query_syntax_uses_the_same_filter_machinery`), e2e
  `typing_filters_the_list_when_the_setting_says_so`, `typing_jumps_to_a_name_by_default`.
* ✅ Sticky selection (*Sticky selection*, <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>S</kbd>): a plain
  click adds or removes an item, as Ctrl+click does; the status bar shows 📌 while it is on, and
  Esc, the indicator or the command turns it off, keeping the picks — `pane.rs`
  (`sticky_selection_makes_a_plain_click_toggle`, `escape_clears_filter_then_sticky_then_selection`),
  e2e `sticky_selection_toggles_items_with_plain_clicks`.
* ✅ Folders that open in place (details view): a folder shows a disclosure arrow; click it, or
  press Right or Left, to open or close it (Right again steps into an open folder, Left on a
  nested item goes to its folder). Children are listed lazily by a shared pool of four listing
  workers and streamed in batches, like the folder itself (a closed folder's queued job is
  dropped; a folder that cannot be read says so on its row); each level is sorted on its own, and the filter
  applies to nested items while keeping the folders that lead to a match. Selection, drag-out,
  open, rename, delete and copy work on nested items (they are ordinary paths); when a folder and
  items inside it are both selected, operations and drag-out act on the folder only. Open folders are
  remembered per tab, including across navigation and in the saved session. Leaving details view
  closes the tree, and returning reopens it. Open folders are watched (non-recursively, like
  the current folder) while they are open, and an operation inside one reloads it; Refresh reloads
  every open folder. A refresh keeps the rows, selection and cursor until the new listing is complete, so the
  tree does not flicker. Open folders are not followed through links (no junction loops) —
  `pane.rs` (`flatten_tree_lists_open_folders_depth_first`,
  `flatten_tree_hides_the_children_of_a_hidden_folder`,
  `flatten_tree_filter_keeps_folders_that_lead_to_a_match`,
  `expanded_folders_list_their_children_in_place`, `arrow_keys_open_step_into_and_close_folders`,
  `selection_reaches_nested_items_and_collapsing_drops_them`, `filter_applies_to_nested_items`,
  `leaving_details_view_closes_the_tree_and_returning_reopens_it`,
  `reloading_an_open_folder_keeps_its_rows_until_the_new_listing_is_in`,
  `refresh_keeps_nested_rows_and_forgets_an_open_folder_that_is_gone`,
  `a_selected_folder_covers_the_selected_items_inside_it`,
  `the_shift_select_anchor_stays_on_its_item_when_rows_move`,
  `an_open_folder_that_cannot_be_listed_reports_why`, `many_open_folders_share_the_listing_workers`,
  `refresh_keeps_the_open_folders`), e2e `folders_open_in_place_with_arrows_and_clicks` (arrow
  click, Right/Left, select-all across levels, filter, Recycle a nested file, double-click a
  nested folder, expanded state remembered after navigating back, a file created outside the app
  in a nested open folder appears).
* ✅ Arrow keys, Page Up/Down, Home and End always move the file cursor unless a text field has
  focus, including after a toolbar or sidebar button was clicked, at the edges of the grid and
  the first row of the list, and with the preview panel open; egui no longer moves focus between
  buttons with them. A focused button still answers Tab, Enter and Space — e2e
  `arrows_move_the_file_cursor_after_a_toolbar_toggle_was_clicked`.
* ✅ Folder auto-refresh on new files — `forge-app/src/watcher.rs::detects_new_file_after_debounce`
  (watcher wired in `main.rs` and `actions.rs`). The rendered list refresh is not driven by a test.

## File operations

* ✅ New folder from the prompt — e2e `new_folder_is_created_from_the_prompt`.
* ✅ Rename, then undo — e2e `rename_then_undo_restores_the_original_name`; engine
  `forge-core/src/ops.rs::move_same_volume_rename_and_undo`.
* ✅ Copy and move between panes, byte-identical — e2e
  `copy_to_other_pane_duplicates_files_byte_for_byte` and `move_to_other_pane_removes_the_source`;
  engine `ops.rs::copy_tree_unicode_preserves_mtime_and_readonly`, `move_cross_volume_fallback`.
* ✅ Conflict handling, keep both — e2e `copy_with_conflict_keep_both_writes_a_numbered_copy`;
  engine `ops.rs::conflict_policies`, `racing_destination_is_preserved_and_keep_both_retries`,
  `ask_resolver_apply_to_all_and_cancel`.
* ✅ Permanent delete asks for confirmation; cancelling keeps the file — e2e
  `permanent_delete_removes_the_file_after_confirmation` and
  `permanent_delete_cancelled_keeps_the_file`.
* ✅ Recycle Bin removal (engine) — `forge-win/src/shell.rs::recycle_removes_file_from_disk`
  and `recycle_missing_path_is_an_error` pass. The first test sends a temporary file to the
  real Recycle Bin on each run (see [LIMITATIONS.md](LIMITATIONS.md)). The e2e suite
  deliberately does not use the Recycle Bin.
* ✅ Duplicates re-verified on disk before recycling — `forge-app/src/tools_ui.rs::recycle_reverifies_against_disk`
  and `duplicate_selection_safety_refuses_to_remove_the_last_copy`.
* 🟡 Open files, Open With, Properties, and reveal in Explorer — `forge-win/src/shell.rs`.
  Opened files start in their own folder, as in Explorer, so scripts and programs using relative
  paths work — `shell::tests::opened_batch_script_runs_in_its_own_folder` runs a real `.bat`.
  No test drives Open With, Properties, or reveal.
* ✅ Clipboard engine: copy and move as `CF_HDROP` — `forge-win/src/clipboard.rs` tests
  `files_round_trip_for_copy_and_move_including_long_unicode_paths`,
  `setting_files_increments_clipboard_sequence`, `clear_if_owned_files_does_not_clear_a_changed_sequence`,
  and `powershell_reads_file_drop_list` pass. The file-list round trip is what these tests prove; the preferred drop effect is
  set by the same module but is not checked on its own.
* 🟡 Clipboard interoperability with Explorer — pasting Explorer's files into ZXraw Explorer is
  covered by e2e tests that put a real CF_HDROP + Preferred DropEffect on the Windows clipboard:
  `explorer_cut_of_two_folders_pastes_with_ctrl_v`, `explorer_copy_pastes_with_shift_insert_without_a_click`,
  `explorer_cut_after_an_in_app_copy_pastes_the_explorer_items` and `ctrl_shift_v_does_not_paste_files`
  (`forge-app/tests/e2e/core.rs`). Copy/cut in ZXraw Explorer publishes the same formats for
  Explorer to paste, but pasting into Explorer itself is not automated.
* ✅ Copy line: copies and moves run one at a time in a queue (Settings › File operations,
  "Run copies and moves": queue by default, or all at once). Queued jobs wait as "Queued" in
  the Operations panel in run order, with buttons to move a job up or down, start it now,
  cancel it before it starts, and pause or resume the whole queue. Transfers between
  different physical drives still run side by side (`forge_win::physical_disks`; unknown
  drives such as network paths wait for everything ahead). A queued job plans and asks about
  conflicts only when it starts, skips sources that vanished meanwhile ("no longer exists"),
  and closing the app with transfers pending asks first. Engine tests in `forge-jobs`
  (`queue_runs_one_at_a_time_in_order`, `queue_reordering_changes_what_runs_next`,
  `cancelling_a_queued_job_never_runs_it`, `pausing_the_queue_holds_waiting_and_pauses_running_jobs`,
  `start_now_jumps_the_queue_even_when_paused`, `transfers_on_different_disks_run_side_by_side`,
  `parallel_mode_starts_everything`, `a_panicking_transfer_frees_the_queue`); e2e
  `queued_copies_wait_and_a_cancelled_one_never_runs`, `queued_copy_resolves_a_conflict_created_while_it_waited`,
  `queued_copy_skips_a_source_deleted_while_waiting` and `closing_with_queued_copies_warns`.
* 🟡 Operation report UI — `forge-app/src/ops_ui.rs`. Per-job pause/resume works for
  running jobs but has no test of its own; the panel layout is not checked visually.
* ✅ Atomic, no-overwrite publishing of new files — `forge-core/src/paths.rs::never_replaces_and_moves_when_free`
  (uses `rename_no_replace`), `ops.rs::racing_destination_without_keep_both_is_reported_and_preserved`,
  `ingest.rs::ingest_manifest_publish_does_not_replace_a_racing_destination`, and
  `extra_tools_ui.rs::contact_sheet_publish_never_replaces_an_existing_output`.

## Power-user tools

* ✅ Batch rename preview and apply, with collision and problem checks — engine in
  `forge-core/src/rename.rs` (`apply_three_cycle_and_undo`, `apply_swaps_two_files`,
  `collision_reported_when_target_is_not_vacated`); CLI tests `rename_dry_run_changes_nothing_and_apply_renames`
  and `rename_refuses_on_problems`; e2e `batch_rename_with_default_template_numbers_the_selection`.
* ✅ BLAKE3 hashing and verification — CLI tests `hash_verify_roundtrip_detects_corruption` and
  `hash_prints_b3sum_style_lines_without_manifest`; `forge-core/src/hashing.rs`
  (`blake3_matches_reference`, `manifest_roundtrip_and_corruption`). Full-file throughput is in
  [BENCHMARKS.md](BENCHMARKS.md).
* ✅ Duplicate detection — CLI test `dupes_finds_pair_and_deletes_nothing`;
  `forge-core/src/duplicates.rs` (`staged_duplicate_detection`, `cancel_duplicates`). The
  200,000-file benchmark tree yields the expected 20 duplicate groups with zero errors.
* ✅ Folder size analysis — `forge-core/src/disk_usage.rs` (`tree_totals_top_files_and_extensions`,
  `cancel_scan`). Not benchmarked.
* ✅ Folder sizes in the Size column — `forge-core/src/dir_size.rs` (`nested_totals_count_bytes_files_and_folders`,
  `junctions_are_not_followed_or_counted`, `cancellation_returns_cancelled`, `cache_lookup_replaces_a_subtree`,
  `store_receives_every_completed_sub_folder`, `cloud_folder_not_on_this_pc_is_not_entered`,
  `unreadable_sub_folder_makes_the_result_partial`), `forge-app/src/folder_sizes.rs`
  (`pending_until_the_walk_answers_then_known`, `a_folder_no_longer_wanted_stops_its_walk`,
  `a_change_below_a_watched_root_updates_its_total`, `remeasure_reuses_trusted_sub_totals`,
  `network_folders_are_off_while_network_sizes_are_off`, `disabled_sizes_are_off_and_measure_nothing`),
  `listing::tests::a_file_still_being_written_is_listed_with_its_live_size` (files still being written
  show their live size instead of 0 B), e2e `browser::folder_totals_appear_in_the_size_column` and
  `browser::a_large_folder_total_shows_quickly` (10,000 files). Folders in the details view show their
  recursive total in the Size column, measured in the background (`…` while measuring, a `+` suffix when
  part could not be read or is a cloud folder not on this PC, hover for file and folder counts). One
  enumeration pass per folder (`FindFirstFileExW`), several threads, totals cached for every sub-folder,
  work for folders no longer on screen cancelled, and a change watch that re-measures only the changed
  branch. Symlinks and junctions are never followed; cloud placeholders are never downloaded; network
  drives are off unless Settings › Browsing › *Also on network drives* is ticked; the whole feature can
  be turned off with *Folder sizes in the Size column*. Sorting by Size uses the totals. Rough numbers
  from one PC (release build, warm cache), not in [BENCHMARKS.md](BENCHMARKS.md): 327k files / 45k folders
  in 0.47 s, 17k files in 14 ms.
* ✅ Compare folders (engine) — `forge-core/src/compare.rs` (`compare_and_update_never_deletes`,
  `mirror_makes_identical`, `non_mirror_plan_with_deletes_is_refused`).
* ✅ Two-way folder sync (engine) — `forge-core/src/compare.rs`: copies each side's new files
  and the newer copy of changed files in both directions and never deletes. Tests cover both
  directions, newer-wins on each side, same-time conflicts skipped without overwriting, type
  mismatches skipped, and a stale preview (source or destination changed) refused.
* 🟡 Compare and sync folders in the UI — mode picker (Update / Mirror / Two-way) with a dry-run
  preview of every action and its direction; *Sync folders…* opens it on the two panes. E2e
  `two_way_copies_each_sides_only_file_to_the_other_side` drives a two-way sync through the
  testhook; the rest of the window is not automated.
* 🟡 Verify backup (compare by content) — `forge-core/src/backup_verify.rs` classifies files as
  match, missing, differs, only in the backup, moved (by BLAKE3, optional) and not checked
  (online-only cloud files are never read). Quick mode trusts equal size and date and hashes
  only on a mismatch; *Full verify* hashes every pair. Unit tests cover the classification,
  quick versus full, moved-file detection with a same-size decoy, placeholders, cancellation,
  case-insensitive matching and the CSV/text export. The window is in `forge-app/src/tools_ui.rs`
  (*Verify backup…*, or the button in *Compare folders*); e2e `tools::` scenarios drive it.
* 🟡 Find and replace in files — `forge-core/src/text_replace.rs` with the preview window in
  `forge-app/src/replace_ui.rs`. Literal or regex search, match case, whole word, include and
  exclude patterns, recursion. Only known text file types are searched unless *Include files*
  names another type; binary files (NUL bytes or known RAW/image/video/archive extensions),
  files above a size limit (16 MiB by default, checked on the bytes read), read-only files and
  online-only cloud files and hard-linked files are skipped. A file whose folder was swapped for
  a junction or link after the preview is refused, by apply and by undo. UTF-8 (with or without
  BOM) and UTF-16 LE/BE are kept, as are CRLF/LF/mixed line endings. Each file is written to a
  temporary file and swapped in with `ReplaceFileW`, which keeps its creation time, attributes,
  ACLs and streams; if it cannot, the file is reported as failed and left untouched (no rename
  fallback). A file edited after the preview is detected on a best-effort basis (see
  [LIMITATIONS.md](LIMITATIONS.md)). The originals are backed up in the profile, and *Undo*
  restores them only if the file is still as the replace left it. The preview shows each
  stored match in full, including matches that span lines (breaks shown as `\n`). Stored
  matches are capped (100 per line, 2,000 per file, 50,000 per scan, none longer than 4 KiB),
  and a scan stops after 50,000 files or 1 GiB read. Matches past a cap are still counted, and
  the window says how many are not shown (or how many files were not searched) before you
  press *Replace*. Unit tests cover every encoding, line endings, binary and hard-link skipping,
  regex and whole word, junction swaps, last-moment edits, a failed `ReplaceFileW`, undo, the
  preview caps, and multi-line and newline-only matches; e2e `tools::` scenarios drive the
  window.
* 🟡 Batch export / convert — `forge-app/src/batch_export.rs`: JPEG, PNG, TIFF and lossless WebP,
  never-upscale resize, JPEG quality, naming template (`{name}`, `{n:3}`, `{w}`, `{h}`, `{ext}`),
  RAW via the embedded preview, EXIF orientation applied, no-replace publishing. Unit tests cover
  the template, sanitising, fit maths, in-batch de-duplication, a real resize-to-JPEG export with
  a failing file, PNG/WebP output, and that an existing destination is never overwritten. The
  window itself is not automated.
* 🟡 Delivery presets ("Deliver to client") — `forge-app/src/delivery_presets.rs`,
  `watermark.rs`, `delivery_meta.rs`, driven through `batch_export.rs`: named presets in
  `delivery_presets.json` (three built-ins; save, save as new, duplicate, rename, delete, restore
  built-ins), text or PNG watermark with nine anchors, size, opacity and margin plus a live
  preview, "only picks" / "rating ≥ N" filtering from library annotations, loose files or one zip
  written under a temporary name and renamed, and EXIF policy (none / clean: no GPS, serials,
  owner or maker notes but copyright kept / all). `Cmd::Deliver` (Ctrl+Shift+E) uses the
  last-used preset; the file context menu has *Deliver with ▸ preset*. Unit tests cover the
  preset round-trip and editing, watermark placement and blending, the zip output (including
  cancel and no-replace), GPS/serial stripping with copyright kept in a real exported JPEG, and
  the rating/pick filter. The window and the context menu are not automated.
* 🟡 Batch metadata editor — `forge-app/src/meta_edit.rs` with `forge-media/src/xmp_write.rs`:
  title (`dc:title`), caption (`dc:description`), creator (`dc:creator`), copyright (`dc:rights`)
  and keywords for many files, with a current → new preview. Writes XMP sidecars only; originals
  are never modified. Seven new writer tests (set/read back, remove, escaping and Unicode, other
  languages and unrelated content kept, unchanged updates write nothing) plus form-logic tests.
  Not checked against Lightroom or Bridge reading the new fields.
* 🟡 Open terminal here — `forge-app/src/terminal.rs`, command `OpenTerminal` (Ctrl+Alt+T) and
  `OpenTerminalAdmin`, on the folder and folder-background context menus. Auto picks Windows
  Terminal, then PowerShell 7, Windows PowerShell, cmd (PowerShell first for UNC folders);
  Settings › Browsing chooses one. Launched through `ShellExecuteW` (`forge-win` `launch_console`)
  with a built argument string, never a shell string: Windows Terminal gets `-d` with `;` escaped
  as `\;`, PowerShell gets an `-EncodedCommand` `Set-Location -LiteralPath`, cmd gets only a
  working directory (or `pushd` for UNC). Unit tests (`terminal::tests`) round-trip every
  terminal's arguments through a CommandLineToArgvW parser for folders with spaces, `&`, `%`,
  `^`, `'`, `;`, Unicode, UNC and `\\?\` prefixes; e2e
  `folder_context_menu_opens_a_terminal_in_that_folder` right-clicks a folder and clicks the menu
  item (automated runs record the launch instead of opening a window). Windows Terminal's `\;`
  handling was checked by hand once; the administrator variant was not run.
* 🟡 Git status badges — `forge-app/src/git_status.rs`, on by default (can be turned off in Settings › Browsing):
  modified, new (added or untracked), ignored (dimmed) and conflicted entries, rolled up to
  folders, and the branch in the status bar. Read in-process on one background thread; no
  external program ever runs (no `git.exe`, no fsmonitor, no filter drivers). Every file is
  opened through `git_status/safe_fs.rs`: a share, mapped drive, SUBST drive (checked with
  `QueryDosDeviceW`) or device path is refused before anything is opened; the local drive's
  root is the only open by path, and must be a volume's root (`\\?\Volume{...}\`); every folder from
  there down to the browsed one, and everything in the repository, is opened one name at a time
  relative to an already-checked handle (`NtCreateFile` with `FILE_OPEN_REPARSE_POINT`, and
  `FILE_OPEN_NO_RECALL` for files), refusing any reparse point, hard-linked file, online-only
  placeholder, or handle off the root's volume (64-bit serial) or outside the root, and
  nothing can be opened except from an entry its held parent folder listed (or looked up by
  that exact name), so a placeholder is refused before it is opened; the raw index is checked
  iteratively (path length and depth, cached-tree depth, size and node count) before gix
  decodes it, and only its entries and cached tree are passed on. Every read is capped and
  goes through the handle that was checked. Repository discovery and the scan
  both run on a deadlined thread (at most one abandoned at a time). gix only parses bytes read that way
  (index, config, refs, ignore files, commits and trees); loose objects and packs, with deltas,
  are read by `git_status/odb.rs`. Tracked files are compared with the index from folder
  listings; a same-size file with a new time is hashed from its raw bytes (at most 256 files of
  up to 16 MB). `.git` must be a real folder in the root, so submodules and linked worktrees
  show no badges. Caps: 4 s, a 64 MB / 100,000-entry index, 50,000 folders and files walked,
  `.gitignore` files (256 KB and 4,096 patterns each, 4 MB and 20,000 patterns in all),
  `packed-refs` (16 MB), 256 packs and 256 MB of pack indexes, 32 MB per object and 512 MB
  inflated in all (compressed input capped per object and in all, deadline checked while
  inflating), 200,000 staged changes or 64 MB of their paths, 4 KB paths, and 5 symbolic refs; a repository over any of them is skipped for the
  session. Results are cached per repository and refreshed when the folder watcher fires or
  after 15 s. Unit tests cover lookup and roll-up, repo-root detection, a real repository
  (modified, staged, untracked, ignored), staged changes read from deltified packs, programs
  named in the config never running, and one test per class of hostile layout with real
  junctions in temp folders, each asserting from a recorder that no handle outside the folder
  was opened: junctions on tracked paths, symbolic ref chains (through a junction, and a loop),
  `.git` files and junctions to other repositories, junctions inside configured paths, links
  among loose objects and packs, capped ignore files, `packed-refs` and packs, and a folder
  swapped for a junction after its handle was opened. Mount points aimed at `\\192.0.2.1\x`
  are refused unfollowed; the symlink variants need developer mode and skip without it. Badge
  drawing is not covered by a test.
* ✅ Command palette matching — `forge-app/src/palette.rs::fuzzy`. Keyboard interaction with the
  palette is not automated.
* ✅ Dual-pane file workflows — see File operations (e2e copy, move, and conflict tests).

## Photography browsing, culling, metadata, and integrations

* ✅ Thumbnail generation and cache, correctness — `forge-media` tests `other_formats_thumbnail`
  (formats), `cache_hit_miss_and_mtime_invalidation`, `dedupe_and_priority_and_bad_files`,
  `cancellation_drops_queued_requests`, and `forge-app/src/thumbs.rs::buckets`.
  Throughput is measured only for JPEG (200 x 24 MP files, [BENCHMARKS.md](BENCHMARKS.md)).
  HEIC/HEIF and AVIF are not decoded ([LIMITATIONS.md](LIMITATIONS.md)).
* 🟡 EXIF, XMP reading, image orientation, and embedded RAW JPEG preview extraction —
  `forge-media` tests (`jpeg_exif_metadata`, `tiff_raw_previews_and_metadata`,
  `cr3_previews_and_metadata`, `raf_preview_and_metadata`, `orf_magic_is_handled`,
  `jpeg_thumbnail_orientation_and_exif_thumb`, `corrupt_and_truncated_inputs_never_panic`) pass
  on synthetic data only. Real camera files from every vendor have not been tested.
* ✅ Ratings, pick state, labels, tags, and collections persisted in SQLite — `forge-db`
  (`annotations::batch_annotations`, `collections::manual_collection_crud_and_order`,
  `indexer::annotations_survive_missing_and_relink`); `forge-core/src/query.rs::labels_and_picks_roundtrip`.
* ✅ Tag suggestions and multi-selection tags (logic) — `forge-app/src/tags_ui.rs` (7 tests:
  suggestion ranking, excluded tags, mixed tags for selections, comma splitting) and
  `library.rs::tag_rename_then_annotation_add_share_writer_order`.
* 🟡 Notes on files — the preview panel's *Note* box (`tags_ui.rs`) saves 800 ms after the last
  keystroke, on focus loss, on a selection change, when the panel closes, and at exit (the
  library writer is flushed for up to 7 s; a note still unconfirmed goes to `pending-notes.json`
  and is saved at the next start). A note being edited follows its file when ZXraw renames or
  moves it before the autosave. It shows *Unsaved changes*, *Saving…*, *Saved ✓* only once the
  database confirms the write, or *Not saved* with the error (`tags_ui.rs` tests cover the
  status, the confirmation and the autosave timer). Notes live in the library
  database for any file type (`annotations.notes`; migration 2 clears blank notes and indexes
  files that have one, tested by opening a version-1 database in `forge-db/tests/notes.rs`).
  Items with a note show a ✎ badge in the grid and details views. Plain-text search also matches
  notes; `note:`/`notes:`, `has:notes` and `missing:notes` filter on them (`forge-db/tests/notes.rs`).
  Notes, ratings and tags follow files that ZXraw renames or moves (rename, batch rename, same- and
  cross-volume moves, and undo). Each set of moves is applied in one transaction by
  `Db::relink_many`, which parks every source under a temporary path first, so swaps and cycles
  keep each note on its file (`forge-db/tests/notes.rs`, `library.rs` tests).
  With XMP sync on, photo and RAW notes are written to `xmpDM:logComment`, never to the caption
  in `dc:description` (`xmp_write.rs` and `library.rs` tests). Sidecar writes skip online-only
  cloud placeholders and fail closed when the attributes cannot be read. A sidecar is replaced
  only if it still holds the bytes the merge was based on, checked while holding a handle that
  blocks other writers; otherwise it is re-read and merged again, or a conflict is reported. Not verified in Lightroom/Bridge, and a note found in a sidecar is not imported.
* 🟡 Culling and compare viewer, zoom/pan, and filmstrip — `forge-app/src/viewer/` (`cull`, `zoom`,
  `cache`, `loader`, `decode`, `meta`). Unit tests cover culling logic, zoom math, cache budget,
  and corrupt input; no visual interaction is certified.
* ✅ Corrupt JPEG in the grid and preview does not crash — e2e `corrupt_jpeg_in_grid_and_preview_does_not_crash`.
* 🟡 XMP sidecar sync (opt-in) — writer and merge safety are tested: `forge-media/src/xmp_write.rs`
  (29 tests: lossless merge that keeps unrelated content, BOM and UTF-16 handling, namespace
  alias and custom prefix resolution, same-process serialised updates, external edit during a
  merge is re-merged, backup refresh, temp-file uniqueness). `library.rs` tests cover when sidecars
  are written (only when sync is enabled), failure summaries, and picks not touching sidecars.
  Not verified with Lightroom, Capture One, or darktable, and the cross-process race window
  described in [LIMITATIONS.md](LIMITATIONS.md) remains.

## Video

* 🟡 Parse `ffprobe` metadata and derive representative frames — `forge-media/src/video.rs`.
  Tests `timeout_kills_child`, `output_cap_enforced`, `cancel_and_failure_and_missing`,
  `video_functions_report_tool_missing`, `ffprobe_portrait_hlg`, `ffprobe_vfr_pq_and_legacy_rotate`,
  and `pix_fmt_table` pass against canned tool output. No real `ffprobe` or `ffmpeg` process has
  been run here: FFmpeg is not installed and is not bundled.
* ⬜ In-app video playback — not implemented. The UI states "video playback is out of scope"
  (`forge-app/src/video_ui.rs`); videos open in an external application.
* 🟡 Filmstrip, scrubbing, related clips, and proxy generation — `forge-app/src/video_ui.rs`.
  Unit tests pass for proxy placement (`placing_a_proxy_never_replaces_an_existing_file`,
  `proxy_temp_collision_and_cleanup_never_delete_a_foreign_file`), GoPro chapter and camera
  sequence grouping, related-media lookup, size and time estimates, proxy argument quoting, progress
  parsing, and timecode offsets. Proxy generation with a real FFmpeg has not been run.
* ✅ Download FFmpeg — `forge-app/src/ffmpeg_setup.rs`, `forge-win/src/http.rs`. Offered in
  Settings › Video, the welcome window, the video preview panel, the proxy dialog and the command
  palette. Downloads a pinned build (Gyan Doshi's 9.0.2 essentials build on GitHub) over WinHTTP,
  checks its exact size and SHA-256, installs only `ffmpeg.exe`, `ffprobe.exe` and the licence into
  `<data>	oolsfmpeg-<version>`, and points Settings › Video at it. Unit tests cover the install,
  checksum and size mismatches, a zip without the tools, cancellation and cleanup; e2e scenarios
  `download_ffmpeg_installs_the_tools_and_points_settings_at_them` and
  `download_ffmpeg_with_a_bad_checksum_installs_nothing_and_says_why` drive it through the UI
  against a local server.

## Ingest

* ✅ Plan an import, apply verified copies, and leave the source intact — CLI test
  `ingest_dry_run_copies_nothing_apply_copies_and_verifies`.
* ✅ Ingest rules (engine) — `forge-core/src/ingest.rs`: `ingest_verifies_is_idempotent_and_checks_format_safety`,
  `conflicts_keep_both_and_rerun_recognises_renamed_copies`,
  `already_imported_execution_rejects_same_size_destination_changes`,
  `shallow_format_check_rehashes_content_changed_after_report`, `refuses_overlapping_dest`.
* ✅ Camera-card detection prompt (decision logic) — `forge-app/src/sidebar.rs` tests
  `camera_folder_prompts_and_plain_card_does_not`, `dismissed_card_stays_quiet_until_removed`,
  `removed_and_reinserted_card_discards_the_old_generation`, `removal_before_check_finishes_drops_the_prompt`;
  `ingest_ui.rs::detects_camera_photo_and_video_directories`. The rendered prompt is not automated.
* 🟡 Ingest UI: template editing, preview, progress and report display, and cancellation —
  `forge-app/src/ingest_ui.rs`. Tests cover preview debounce, report clearing when inputs change,
  destination space checks, and a registered ingest job that copies, verifies, and runs the format
  check. The visual workflow is not automated.

## Search

* ✅ Evaluate structured filters over listing entries — `forge-core/src/query.rs` tests
  (`matches_entries`, `three_valued_logic_for_db_only_fields`, `negation_or_and_parentheses`,
  `relative_dates`). The benchmark runs `Query::matches_entry` over 50,000 entries (12,500 matches
  for `ext:jpg OR ext:cr3`) in [BENCHMARKS.md](BENCHMARKS.md).
* ✅ Index folders in SQLite and search indexed metadata — `forge-db/src/search.rs`
  (`every_filter_type`, `sorting_paging_scope_and_offline`, `user_values_are_parameters_not_sql`),
  `forge-db/tests/indexer.rs`, CLI test `search_after_index_finds_file`, and the 200,000-file
  index and search benchmark.
* ✅ Resume an interrupted scan — `forge-db/tests/indexer.rs::interrupted_pass_resumes_from_checkpoint`.
  Interrupted-scan recovery is not benchmarked.
* ✅ Saved searches and smart collections (database path) — `forge-db/src/collections.rs`
  (`smart_collections_and_saved_searches`, `import_reports_smart_collection_query_conflict_without_replacing`).
  The management UI is 🟡 (see Library and collections).

* 🟡 Everything and Windows Search name-search backends — `forge-win/src/ext_search.rs` and the
  selector in the search window. Queries are passed as arguments or an environment variable,
  never spliced into a command line; SQL literal and LIKE escaping are unit-tested, including
  injection attempts. Everything (`es.exe`) was not installed here, so it has not been run live.

## Library and collections

* ✅ Store index roots, offline rows, ratings, tags, collections, and saved searches in the local
  database — `forge-db/src/collections.rs::export_import_roundtrip_with_placeholders`,
  `indexer.rs::deleted_files_become_offline_and_return`, `indexer.rs::relink_simple_prefix_rewrite_then_present_again`.
* ✅ Relink merges annotations, tags, and collection memberships on collision —
  `forge-db/src/files.rs::relink_collision_merges_annotations_tags_and_memberships`,
  `relink_marks_existing_destination_present`, `relink_reports_missing_destination_offline`.
* ✅ Collection import conflicts do not replace existing data —
  `collections.rs::import_reports_smart_collection_query_conflict_without_replacing`.
* ✅ Collection export replaces the destination only after a complete write —
  `forge-app/src/collections_ui.rs::export_replaces_destination_only_after_complete_temp_write`,
  `failed_export_keeps_existing_destination_intact`. Relink preview counts exact prefixes only
  (`relink_preview_counts_exact_prefix_and_not_siblings`).
* 🟡 Library and collection browser, editing, and saved-search management UI —
  `forge-app/src/collections_ui.rs` (reordering items within a collection, picker filtering), with a "How collections work"
  explainer, a guided *New collection…* window (empty / from selection / smart) and a detail
  pane with counts, file preview and actions. User interaction is not automated.

## Archives and extra tools

* ✅ ZIP create, list, and extract (engine) — `forge-core/src/archive.rs`: `create_list_extract_roundtrip`,
  `listing_stops_at_the_entry_cap`, `zip_slip_is_rejected_before_writing`, `zip_bomb_limits`,
  `symlink_entries_skipped`.
* ✅ Extract selected ZIP entries only — `archive.rs::selected_extraction_writes_only_requested_entries_and_checks_all_names`.
* ✅ Contact sheets: pagination, bounded layout, and captions with system fonts —
  `forge-app/src/extra_tools_ui.rs` (`contact_sheet_pagination_uses_rows_times_columns`,
  `contact_sheet_page_loader_holds_at_most_one_page_of_thumbnails`,
  `contact_sheet_layout_is_bounded_and_accounts_for_title_and_captions`,
  `unicode_caption_draws_pixels_with_system_fonts`). Sample sheets have not been reviewed visually.
* ✅ Split and join (engine) — `forge-core/src/split.rs` (15 tests): round trip with Unicode and
  non-multiple sizes, changed and truncated part detection, whole-file hash mismatch publishes nothing,
  cancellation leaves no output, a simulated crash before publish leaves no final name, overwrite
  refused, symlinked parts rejected.
* 🟡 Archive, split/join, and contact-sheet UI workflows — `forge-app/src/extra_tools_ui.rs`.
  The engine is tested; the dialogs and progress display are not automated.
* ⬜ 7z and RAR archives — not implemented.

## Status bar

* ✅ Selection summary: count and total size of the listed, selected entries, including the
  measured totals of selected folders (shown as "so far…" while still measuring)
  (`forge-app/src/app.rs::selection_summary_tests`, including
  `a_selected_folder_adds_its_measured_total_and_says_so_while_it_is_measured`), and the total running time of selected
  video and audio files, probed with `ffprobe` in the background and cached by path and
  modification time (`forge-app/src/media_durations.rs`; tests cover totals, the 500-file probe
  cap and the partial / unknown wording). Not run live here because FFmpeg is not installed.

## Performance

* ✅ Streaming directory listing in the engine (`list_dir_streaming`) — 50,000-entry runs in
  [BENCHMARKS.md](BENCHMARKS.md).
* 🟡 Listing off the UI thread, and the bounded duplicate-hash worker pool (8 partial-hash and 4
  full-hash I/O threads, `forge-core/src/duplicates.rs`) — the duplicate scan returns correct results
  in the benchmark, but thread behaviour and the UI-thread split are not measured.
* 🟡 Virtualised table and grid painting — `egui::show_rows` is used in `forge-app/src/views.rs`
  (details list at line 878, thumbnail grid at line 1166); frame pacing is not measured.
* ✅ Thumbnail disk cache, DB indexing, and file hashing — measured by the benchmark harness under
  concurrent build load. Numbers are not regression baselines.

## UI/UX

* 🟡 egui desktop shell, list/grid modes, keyboard commands, themes, preview pane — source is
  present and commands are driven by the e2e scripts; visual review is not complete.
* ✅ App icon colour (Settings › Appearance: ten colours, Graphite by default; independent of the UI accent) — `forge-app/src/app_icon.rs`.
  The freestanding folder-camera mark is drawn per colour by `packaging/draw_logo.py`, with a
  simplified mark at 16–24 px; every colour is embedded in `zxraw.exe` (Graphite is the file
  icon). Old "Square"/"Round" settings load as Graphite. The choice switches the window/taskbar
  icon live and repoints the user's own shortcuts to the app (Start menu, desktop, pinned
  taskbar) — `forge-win/src/shell.rs::shortcut_icons_change_only_for_shortcuts_to_the_app`,
  `app_icon::tests::exe_icon_resources_match_the_window_icons`.
  Pinned taskbar icons can lag until Explorer restarts.
* ✅ Customisable keyboard shortcuts (Settings › Keyboard shortcuts): search, record a new
  combination, clear, reset one or all; conflicts offer to move the binding. Overrides are
  stored by stable command id and used for matching and every displayed hint (menus, palette,
  tooltips) — `forge-app/src/commands.rs` (`parse_shortcut`/`shortcut_text` tests).
* ✅ Settings button (⚙) in the top bar, alongside the palette button and <kbd>Ctrl</kbd>+<kbd>,</kbd>.
* ✅ Per-type file icons for code, documents, spreadsheets, executables, archives and config
  files — `views::plain_kind_tests`.
* ✅ "Make ZXraw Explorer my default" reports types Windows wants confirmed as a short notice
  (details go to the log) instead of a raw error list — `forge-app/src/default_app.rs`.
* ✅ Accent colour (Settings › Appearance: Blue, Purple, Pink, Red, Orange, Green, Teal) drives
  selection, focus rings, links, tab/sidebar indicators, progress bars; the
  neutral surfaces are faintly tinted towards it — `forge-app/src/theme.rs`.
* ✅ Colour contrast meets WCAG AA for selected rows, accent text and links, and focus rings, for
  every accent in both themes — `forge-app/src/theme.rs` (`selected_row_text_meets_wcag_aa`,
  `accent_text_meets_wcag_aa_on_every_surface`, `focus_ring_meets_non_text_contrast_on_selection_and_panel`).
  Selection and focus colours come from one source.
* ✅ AccessKit tree for the details list — `forge-app/src/views.rs::details_rows_emit_named_selected_accesskit_nodes`.
* ✅ Keyboard-adjustable column splitter exposes its value and range to AccessKit —
  `views.rs::column_splitter_exposes_current_value_and_allowed_range`. Key-press resizing is not separately tested.
* 🟡 Screen reader and other assistive technology — the AccessKit nodes are checked by unit tests
  only. No real assistive technology (for example NVDA or Narrator) has been run.
* ✅ Automation hook — `forge-app/src/testhook.rs` runs step scripts (`ZXRAW_SCRIPT`) with filesystem
  assertions; the 10-scenario e2e suite passes. The hook asserts files, not pixels, and does not simulate
  pointer or keyboard input. The `ZXRAW_SCREENSHOT` and `ZXRAW_CMDS` smoke modes remain.

## Reliability, privacy, and packaging

* ✅ Links and reparse points are not followed by recursive operations — `forge-core/src/ops.rs`
  (`links_are_not_followed`, `links_are_not_followed_windows`), `listing.rs::junctions_are_links`,
  `forge-db/tests/indexer.rs::junctions_are_recorded_but_not_followed`.
* ✅ Cloud placeholders are never downloaded by background work. OneDrive, Dropbox and Google
  Drive files that are only online (`RECALL_ON_DATA_ACCESS`, `RECALL_ON_OPEN` or `OFFLINE` in the
  listing attributes) get a cloud badge instead of a thumbnail. Thumbnails, preview-pane EXIF and
  text, the video panel, video durations, library metadata extraction, XMP sidecars, duplicate and
  similar-photo scans, folder compare and its sync preview, the ingest preview, and viewer
  prefetching all skip them through one check,
  `forge_core::cloud::background_read_allowed`. The video panel's workers (probe, filmstrip,
  scrub frames, each proxy encode) and checksum Skip use `cloud::open_for_background_read`
  instead: checked again on the open handle, which is held so the file cannot be dehydrated
  mid-read. Opening a file still downloads it. A copy (or a move to another drive) of 10 or
  more online-only files, or 1 GB or more, asks first and gives the count and size; the check
  runs off the UI thread for files and folders alike, can be cancelled, and a check that could
  not cover everything always asks. The checksum tool counts online-only files (and an
  online-only manifest) first and asks: download and hash, skip them, or cancel
  (`hashing.rs::online_only_files_are_skipped_or_downloaded`,
  `an_online_only_manifest_is_not_read_without_consent`, e2e
  `tools::checksums_ask_before_hashing_online_only_files`). Tests: `forge-core/src/cloud.rs`, `duplicates.rs::online_only_files_are_never_compared`,
  `compare.rs::online_only_files_are_not_hashed_by_folder_compare`,
  `forge-media/tests/media.rs::background_thumbnails_never_read_online_only_files`,
  `forge-app` `library::tests::metadata_extraction_skips_online_only_photos` and
  `ops_ui::cloud_warning_tests`. Real placeholders cannot be created in tests, so they are
  simulated; the badge has not been checked against a live OneDrive folder.
* ✅ Copies stage to a temporary name and are cleaned up on cancel — `ops.rs::cancel_mid_copy_leaves_no_partial_files`.
* ✅ ZIP extraction rejects unsafe paths before writing — `archive.rs::zip_slip_is_rejected_before_writing`.
* ✅ SQLite migrations, WAL and integrity, and corrupt-database recovery — `forge-db/src/lib.rs`
  (`migrations_are_idempotent`, `corrupt_file_is_moved_aside_and_recreated`,
  `damaged_header_of_real_database_is_recovered`) and `forge-db/tests/indexer.rs`.
* ✅ Worker panics are caught; jobs support cancellation — `forge-jobs`
  (`pool_runs_tasks_and_survives_panics`, `child_token_inherits_cancel`, `bounded_queue_evicts_low_priority`).
* ✅ Crash reports — `forge-app/src/crash.rs` (`deliberate_harness_panic_writes_a_report_and_retains_only_twenty`,
  `pending_report_is_returned_once_per_startup`, `pending_report_is_suppressed_when_startup_marker_cannot_be_written`).
  The startup notice in the rendered app is not exercised.
* ✅ Test runs use their own profile — every e2e run passes `ZXRAW_PROFILE_DIR` and a sandbox folder;
  without it the hook uses a throwaway profile (`forge-app/src/settings.rs`, around line 111). Session restore in
  tests therefore does not read the user's tabs.
* ✅ Windows shell registration lifecycle — `forge-win/src/integration.rs` tests run against an isolated HKCU
  test key (`registration_lifecycle_uses_an_isolated_hkcu_test_root`). The Explorer context-menu entries are not
  visually checked.
* ✅ Default photo and RAW app (opt-in, first-run welcome and Settings > Default app) — `forge-win/src/file_types.rs`
  registers per-user ProgIDs, "Open with" entries, `Capabilities`/`RegisteredApplications`, and per-user class
  defaults. The UserChoice v1 hash implementation matches read-only values for the current user's `.jpg`, `.cr3`,
  `.png`, and `.txt` associations. An ignored Windows test wrote a dummy extension and confirmed that
  `AssocQueryStringW` honors its valid hash and falls back for an invalid hash. Isolated HKCU tests cover
  registration restore. A photo passed on the command line
  opens in the viewer with its folder (`a_photo_passed_on_the_command_line_opens_in_the_viewer_with_its_folder`).
  The Default apps fallback page was not exercised.
* ✅ Default filesystem folder app (opt-in, same flow) — `forge-win/src/integration.rs` sets only the `Directory`
  and `Drive` shell defaults, leaving `Folder` untouched. An ignored Windows test verified that both associations
  resolve to the built ZXraw executable, then restored the real HKCU values and confirmed the original defaults,
  verb entries, and backup state were restored.
* 🟡 Packaging — `packaging/build-release.ps1`, `packaging/zxraw.iss`, `packaging/generate_icon.py`,
  `packaging/generate-third-party.ps1`, and the VERSIONINFO resource in `crates/forge-app/build.rs`.
  Commit `e3bfbd2` records a release zip that was built and whose executable launched; that build is not
  reproduced in this pass and no `dist/` output exists in this checkout. The Inno Setup installer has not
  been compiled (`ISCC` is not installed). The release script builds with `--no-default-features`, which
  removes the automation hook; this was not checked in a release binary here.
* 🟡 Third-party inventory and licence notes — `packaging/THIRD_PARTY.md` lists the runtime dependencies for
  the Windows target and flags three for licence review. The notices those licences require are not yet
  shipped (see [LIMITATIONS.md](LIMITATIONS.md)).
* 🟡 Local-first, no-telemetry direction is stated in `README.md`; no network-monitoring audit has been performed.
  All network code goes through `forge-win/src/http.rs` (WinHTTP). It is used by the daily update check (on by
  default; Settings › Updates), the user-clicked update download, and the user-confirmed FFmpeg download.
* ✅ Update check and one-click update — `forge-app/src/updater.rs` asks the GitHub releases API at most once a
  day and shows a notice for a newer stable release. *Update now* downloads `SHA256SUMS.txt` and the installer
  only from the project's release URLs (and GitHub's download CDN after redirects), checks size and SHA-256,
  then runs the installer silently with `/RELAUNCH` and closes the app. Portable copies open the release page
  instead. Unit tests cover version comparison, release parsing, asset selection, checksum lists, the URL
  allow-list and the daily throttle; e2e scenarios (`tests/e2e/system.rs`) serve a fake release from a local
  listener and check that a matching installer is accepted and a tampered one refused (no installer is run).

## Recently hardened

These areas changed in review-fix commits (`91407a3` to `dfd43d9`). Each is covered by the tests named above
where a test exists:

* Atomic, no-overwrite publishing of files, contact sheets, archives, and split parts (`rename_no_replace`).
* Split and join: publish, cancellation, and changed-part handling; ZIP selected extraction and path checks.
* XMP writer integrity: namespace binding, Unicode and UTF-16 sidecars, serialised updates, external edits during a merge.
* Collections: relink merges annotations, tags, and memberships; import conflicts keep existing data; export
  replaces its destination only after a complete write.
* Tags: ordered tag operations; unresolved annotation paths report an error and revert the optimistic cache.
* Ingest: stale reports are cleared; already-imported checks reject changed destinations.
* Automation: hook is gated behind debug builds or the `automation` feature; script paths are sandboxed and
  Recycle Bin commands are rejected before dispatch (`testhook.rs` tests).
* Context-menu refresh after registration changes, licence notices (`packaging/`), and accessibility
  (selection and focus colours, column splitter).
* Crash reports on panic and test-profile isolation.
