# Implementation plan & TODO

Status legend: ✅ done & verified · 🟡 partial · ⬜ not started. Only the lead updates ✅
after building and testing the change.

## Team / ownership (wave 1)

Each work package owns a disjoint set of files. Agents must not edit files outside their
package; shared files (root `Cargo.toml`, `forge-core/src/{lib,model,listing,error}.rs`,
`forge-jobs`) are owned by the lead.

| Pkg | Owner | Files |
|---|---|---|
| WP-A FS engine | Opus sub-agent | `forge-core/src/{ops,paths,hashing,duplicates,compare,disk_usage,archive,ingest}.rs` |
| WP-B Database & search | Opus sub-agent | `forge-db/**`, `forge-core/src/query.rs` |
| WP-C Media pipeline | Opus sub-agent | `forge-media/**` |
| WP-D Windows shell + rename | Haiku sub-agent | `forge-win/**`, `forge-core/src/rename.rs` |
| WP-E Desktop UI | Lead (Opus) | `forge-app/**` |
| WP-F CLI | Haiku sub-agent (wave 2) | `forge-cli/**` |
| Reviews | GPT-6-Luna (Codex) | read-only adversarial review per wave |

## Phases

1. Foundation — shell, tabs, browsing, file ops, logging, settings, tests.
2. Power user — dual pane, batch rename, operation queue, search, disk usage, duplicates.
3. Media engine — indexing, thumbnail cache, image/video metadata, previews.
4. Photography — culling, ratings, compare, collections, contact sheets.
5. Videography — scrub/filmstrip, technical metadata, grouping, proxies.
6. Ingest — templates, verified copy, reports.
7. Polish — accessibility, installer, benchmarks, docs.

See `docs/FEATURES.md` for the per-feature checklist.

## feature/finish-remaining (lead: Opus 5.5)

Worktree branch for proving the 🟡 workflows end to end and finishing the remaining scope.
Status uses the legend above; ✅ only after the lead has rebuilt, run the tests and looked at
the screenshots.

### Wave 0 — test infrastructure
* ✅ Script verbs per area (`ui <area> …`), real key/mouse injection, dialog seam, Recycle Bin
  sandbox seam, frame-time recorder, e2e suite split per area (`2fe19c2`).

### Wave 1 — prove the 🟡 workflows (GPT-6-Luna max, one agent per area)
* ✅ Viewer: culling keys persist ratings across restart; corrupt media in viewer/video
  paths; cancelling thumbnail generation (`1454c2d`).
* ✅ Ingest: fake card through the UI with verified copies and safe-to-format; source
  vanishing mid-copy (fault injection) for copy, move and ingest (`cc877d5`).
* ✅ Library: collections create/add/relink; search after indexing; tags; restart restores
  settings and resumes an interrupted index (`36a1ea4`).
* ✅ Tools: zip create/extract (hostile zips), split/join, duplicates → recycle (sandbox),
  compare/sync Update mode, auto-refresh after external changes (`2aada08`).
* ✅ 50k-folder responsiveness: release frame times in BENCHMARKS.md; e2e perf scenarios guard
  against per-frame O(n) work.
* ✅ Adversarial review of wave 1 (13 findings) and fixes (`f5708aa`, `fb12b68`, `dc0b452`,
  `b5ccc13`).

### Wave 2 — missing features
* ✅ Near-duplicate / burst grouping in the browser (`35c832b`).
* ✅ GPS panel (offline) + "open in maps" link-out (`87f8cc1`).
* ✅ Metadata report export (CSV/JSON) (`87f8cc1`).
* ✅ Locate proxies for videos (`a9850df`).
* ✅ Associate exports with originals, configurable filename rules (`a9850df`).
* 🟡 OLE drag-out to other apps (`75f79b1`): implemented and unit tested; the drop into a
  real external app is manually unverified.
* ✅ HEIC/AVIF thumbnails and viewer via WIC (`75f79b1`).
* ✅ Adversarial review of wave 2 in three slices (Haiku 5.5 high; 29 findings) and fixes
  (`dbf3bc8`, `5e2ca91`, `f78ca01`, `45fa89c`, `06f7efa`, `297c61e`).
* ✅ User-reported: double-click/Enter on folders failed at fractional display scaling;
  fixed with real-pointer e2e regressions (`05c17e4`).

### Wave 3 — packaging polish
* 🟡 Inno Setup installer: `packaging/zxraw.iss` exists, but ISCC is not installed on the build
  machine, so the installer was not compiled on this branch (see LIMITATIONS.md).
* ✅ First-run experience (`67a5147`).
* ✅ In-app crash-report notice (`67a5147`).
