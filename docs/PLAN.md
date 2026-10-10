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
