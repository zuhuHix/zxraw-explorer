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
  `copy_to_other_pane_duplicates_files_byte_for_byte`). The address bar, breadcrumb UI, and
  drive sidebar are not automated.
* ✅ Stream directory entries in batches (engine) — `forge-core/src/listing.rs`
  (`lists_unicode_and_streams_batches`, `junctions_are_links`). The 50,000-entry run in
  [BENCHMARKS.md](BENCHMARKS.md) completed with zero listing errors.
* ✅ Cancel a listing (engine) — `forge-core/src/listing.rs::cancel_stops_listing`. Cancelling
  a superseded listing from the rendered UI is not exercised.
* 🟡 Browse a 50,000-file folder in the desktop UI — the screenshot hook loaded the folder and
  captured a frame in an earlier run (not repeated for `dfd43d9`); see [BENCHMARKS.md](BENCHMARKS.md).
  Frame pacing and interactive responsiveness are not measured.
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
* 🟡 Clipboard interoperability with Explorer — paste into and out of Explorer has not been checked.
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
* 🟡 Batch export / convert — `forge-app/src/batch_export.rs`: JPEG, PNG, TIFF and lossless WebP,
  never-upscale resize, JPEG quality, naming template (`{name}`, `{n:3}`, `{w}`, `{h}`, `{ext}`),
  RAW via the embedded preview, EXIF orientation applied, no-replace publishing. Unit tests cover
  the template, sanitising, fit maths, in-batch de-duplication, a real resize-to-JPEG export with
  a failing file, PNG/WebP output, and that an existing destination is never overwritten. The
  window itself is not automated.
* 🟡 Batch metadata editor — `forge-app/src/meta_edit.rs` with `forge-media/src/xmp_write.rs`:
  title (`dc:title`), caption (`dc:description`), creator (`dc:creator`), copyright (`dc:rights`)
  and keywords for many files, with a current → new preview. Writes XMP sidecars only; originals
  are never modified. Seven new writer tests (set/read back, remove, escaping and Unicode, other
  languages and unrelated content kept, unchanged updates write nothing) plus form-logic tests.
  Not checked against Lightroom or Bridge reading the new fields.
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
* 🟡 Notes editor — the notes field saves on Enter or focus loss (`tags_ui.rs`), but no test
  covers it and the UI flow is not automated. Notes are stored in the app library and are not
  written to XMP.
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
  `forge-app/src/collections_ui.rs` (reorder, picker filtering), with a "How collections work"
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

* ✅ Selection summary: count and total size of the listed, selected entries
  (`forge-app/src/app.rs::selection_summary_tests`), and the total running time of selected
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
